//! NexLink PC app — a tray-only app (Windows and Linux builds).
//!
//! The app lives in the tray / notification area. Its one small window
//! holds the transport settings and the Connect button. Connecting is
//! always manual — there is no auto-retry anywhere, by design.
//!
//! Connect does the real thing now (backlog item 2): after the phone
//! check (armed), it opens the virtual adapter (Wintun on Windows, a
//! TUN device on Linux), installs the tunnel routes, and runs the
//! nxcore engine on its own thread, so all PC traffic flows through
//! the phone. Disconnect — or quitting the app — stops the engine,
//! removes the routes and stops the adapter.
//!
//! Since v0.3.0, "Connected" is earned, not asserted: the engine first
//! pushes a real DNS query and a real TCP open through the tunnel and
//! only then may the UI show Connected (with live byte counters); the
//! routes are bound to the adapter and read back after install, and
//! the adb forward is supervised for the life of a USB tunnel.
//!
//! The app must run privileged: creating the adapter and changing
//! routes need it. On Windows the installed app elevates itself — the
//! manifest (backlog item 3, `windows-app-manifest.xml`) asks Windows
//! for administrator rights at launch; a hand-built exe still needs
//! right-click → Run as administrator. On Linux the app must be
//! started as root (e.g. via sudo) — there is no self-elevation yet,
//! and Connect reports it plainly when it is missing.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

/// Wintun adapter backend (Windows only): creates the "NexLink" adapter
/// and pumps packets to/from the engine's channels.
#[cfg(windows)]
mod wintun_backend;

/// Windows route + DNS management for the tunnel (item 2).
#[cfg(windows)]
mod routes;

/// "Start with Windows" via the HKCU Run key (item 3).
#[cfg(windows)]
mod autostart;

/// "Start at login" on Linux via an XDG autostart entry (item 3's
/// counterpart).
#[cfg(target_os = "linux")]
mod autostart_linux;

/// The platform adapter backend under one name: the Wintun backend on
/// Windows, the `nxtun` TUN backend on Linux. Both export `Device`,
/// `Channels` and the addressing constants with identical shapes, so
/// the tunnel code below is written once against either.
#[cfg(windows)]
use wintun_backend as adapter;
#[cfg(target_os = "linux")]
use nxtun as adapter;

/// The platform route + DNS manager under one name. Both expose
/// `install(transport, phone_ip)`, `TunnelRoutes::remove` and
/// `is_elevated` with the same signatures.
#[cfg(windows)]
use routes as net_routes;
#[cfg(target_os = "linux")]
use nxtun::routes as net_routes;

/// Pure network helpers, unit-tested on every platform. Only used by
/// the Windows routes module — hence the allow on other platforms.
#[allow(dead_code)]
mod netutil;

use std::io::Read;
use std::net::{Ipv4Addr, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant as StdInstant};

use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Emitter, Manager, RunEvent, WindowEvent};

use nxcore::engine::{client_handshake, TrafficCounters};
use nxcore::proto::DEFAULT_PORT;

/// The per-user state directory the log lives in: `%APPDATA%` on
/// Windows, `$XDG_STATE_HOME` (default `~/.local/state`) on Linux,
/// the temp directory as a last resort on either.
fn state_dir() -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            return PathBuf::from(appdata);
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Some(xdg) = std::env::var_os("XDG_STATE_HOME") {
            return PathBuf::from(xdg);
        }
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(".local").join("state");
        }
    }
    std::env::temp_dir()
}

/// Always-on diagnostics: point the nxcore log at
/// `<state dir>/NexLink/nexlink.log` so every run — successful or
/// not — leaves a trace on the user's PC (v0.2.1 logged only to
/// stderr behind an env var, so its failing run left nothing to
/// diagnose). `NXLOG` still adds stderr output on top, for
/// development.
fn init_logging() {
    let path = state_dir().join("NexLink").join("nexlink.log");
    let _ = nxcore::set_log_file(&path);
    nxcore::nxlog(format!("NexLink {} starting", env!("CARGO_PKG_VERSION")));
}

/// One live tunnel: the engine thread plus the flag that stops it.
/// The thread owns the engine, the adapter device and the routes, and
/// tears them all down when the engine exits — whether it was asked to
/// stop or the link died on its own.
struct ActiveTunnel {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
    summary: String,
    /// Live DATA byte counters, shared with the engine (status line).
    counters: TrafficCounters,
    /// The transport the tunnel was opened with ("usb" / "wifi") —
    /// the status poll supervises the adb forward for USB tunnels.
    transport: String,
}

/// Shared tunnel state, managed by Tauri. Cheap to clone (Arcs inside),
/// so commands can move it into `spawn_blocking`.
#[derive(Clone)]
struct TunnelState {
    inner: Arc<Mutex<Option<ActiveTunnel>>>,
    /// The last tunnel error (engine died / link lost). Cleared on a
    /// successful connect; survives disconnect so the UI can show it.
    last_error: Arc<Mutex<Option<String>>>,
    /// When the status poll last verified the adb forward (USB
    /// supervision is throttled; the poll itself runs every 2 s).
    last_adb_check: Arc<Mutex<Option<StdInstant>>>,
}

impl TunnelState {
    fn new() -> Self {
        TunnelState {
            inner: Arc::new(Mutex::new(None)),
            last_error: Arc::new(Mutex::new(None)),
            last_adb_check: Arc::new(Mutex::new(None)),
        }
    }

    /// Stop the engine thread (if any) and clear the state. Always
    /// succeeds; used by the Disconnect command and on app exit.
    fn stop_tunnel(&self) -> String {
        let tunnel = self.inner.lock().unwrap().take();
        let Some(tunnel) = tunnel else {
            return "Not connected.".to_string();
        };
        tunnel.stop.store(true, Ordering::Relaxed);
        let _ = tunnel.thread.join();
        nxcore::nxlog("tunnel stopped".to_string());
        match self.last_error.lock().unwrap().take() {
            Some(e) => format!("Disconnected. Note: the tunnel had already died: {e}"),
            None => "Disconnected — routes removed, adapter stopped.".to_string(),
        }
    }
}

/// The adb binary's locations next to the exe: the shipped bundle
/// first (the Windows installer puts adb.exe and its DLLs in
/// `resources/` next to the exe — same layout as wintun.dll; a Linux
/// package would put `adb` in the same places), then next to the exe
/// itself (hand-built runs).
#[cfg(windows)]
fn adb_candidates(dir: &std::path::Path) -> [PathBuf; 2] {
    [dir.join("adb.exe"), dir.join("resources").join("adb.exe")]
}

/// See the Windows version. (No Linux bundle ships adb yet, so on
/// Linux this normally falls through to PATH.)
#[cfg(not(windows))]
fn adb_candidates(dir: &std::path::Path) -> [PathBuf; 2] {
    [dir.join("adb"), dir.join("resources").join("adb")]
}

/// Find adb: the bundled copy first (see [`adb_candidates`]), then on
/// PATH (a developer's own platform-tools).
fn adb_command() -> Command {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for candidate in adb_candidates(dir) {
                if candidate.exists() {
                    return Command::new(candidate);
                }
            }
        }
    }
    Command::new("adb")
}

fn run_adb_forward() -> Result<(), String> {
    let output = adb_command()
        .args(["forward", &format!("tcp:{DEFAULT_PORT}"), &format!("tcp:{DEFAULT_PORT}")])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("Could not run adb ({e}). Is USB debugging on and the phone plugged in?"))?;
    if output.status.success() {
        Ok(())
    } else {
        let mut msg = String::new();
        let _ = output.stderr.as_slice().read_to_string(&mut msg);
        Err(format!("adb forward failed: {}", msg.trim()))
    }
}

/// Is our `adb forward` still in the adb server's list? The forward
/// lives in the adb server, not in this process: another platform-tools
/// install restarting that server (or security software killing it)
/// makes it silently disappear, and v0.2.1 never noticed. Returns
/// `None` when adb itself cannot be run or listed — a broken tool is
/// not proof of a lost forward, so the caller only acts on `Some`.
fn adb_forward_active() -> Option<bool> {
    let output = adb_command()
        .args(["forward", "--list"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Some(netutil::adb_forward_present(&text, DEFAULT_PORT))
}

/// The name this build introduces itself with in the NXLP handshake
/// (the phone displays it in its connection state).
const CLIENT_NAME: &str = if cfg!(windows) {
    "NexLink for Windows"
} else if cfg!(target_os = "linux") {
    "NexLink for Linux"
} else {
    "NexLink"
};

/// Reach the phone and do the NXLP handshake. Returns the handshaken
/// link stream (kept open — the engine takes it over), the phone's
/// hello, and the phone's IP on Wi-Fi (needed for the /32 host route).
fn reach_phone(
    transport: &str,
    address: &str,
) -> Result<(TcpStream, nxcore::engine::HelloAckInfo, Option<Ipv4Addr>), String> {
    let (target, phone_ip): (String, Option<Ipv4Addr>) = match transport {
        "usb" => {
            run_adb_forward()?;
            (format!("127.0.0.1:{DEFAULT_PORT}"), None)
        }
        "wifi" => {
            let ip = address.trim();
            if ip.is_empty() {
                return Err("Enter the phone's Wi-Fi IP address first.".to_string());
            }
            let parsed: Ipv4Addr = ip
                .parse()
                .map_err(|_| format!("'{ip}' is not a valid IPv4 address."))?;
            (format!("{ip}:{DEFAULT_PORT}"), Some(parsed))
        }
        "bluetooth" => {
            return Err("The Bluetooth dialer lands in the next milestone — USB and Wi-Fi first.".to_string());
        }
        other => return Err(format!("unknown transport: {other}")),
    };

    let mut stream = TcpStream::connect_timeout(
        &target.parse().map_err(|_| format!("bad address: {target}"))?,
        Duration::from_secs(4),
    )
    .map_err(|e| {
        format!(
            "Could not reach the phone at {target} ({e}). Is 'Allow connection' on in the NexLink app on your phone?"
        )
    })?;

    let hello = client_handshake(&mut stream, CLIENT_NAME)
        .map_err(|e| format!("The phone answered but the handshake failed: {e}"))?;

    if !hello.armed {
        return Err("The phone answered but 'Allow connection' is off. Turn it on in the NexLink app on your phone, then press Connect again.".to_string());
    }
    Ok((stream, hello, phone_ip))
}

/// Tell the settings window which connect stage we are in (it shows
/// the text verbatim) and record it in the log.
fn emit_stage(app: &tauri::AppHandle, stage: &str) {
    nxcore::nxlog(format!("connect stage: {stage}"));
    let _ = app.emit("connect-stage", stage);
}

/// The Connect button: reach the phone, then start the real tunnel.
/// Runs on a blocking thread — the handshake and the adapter
/// setup both do blocking I/O.
#[tauri::command]
async fn connect(app: tauri::AppHandle, transport: String, address: String) -> Result<String, String> {
    // Clone the *inner* state (Arcs) — `app.state()` only borrows `app`,
    // which would not live long enough for `spawn_blocking`.
    let state = (*app.state::<TunnelState>()).clone();
    let app_handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        connect_blocking(&app_handle, &state, &transport, &address)
    })
    .await
    .map_err(|e| format!("internal error: {e}"))?
}

fn connect_blocking(
    app: &tauri::AppHandle,
    state: &TunnelState,
    transport: &str,
    address: &str,
) -> Result<String, String> {
    if state.inner.lock().unwrap().is_some() {
        return Err("Already connected — press Disconnect first.".to_string());
    }
    state.last_error.lock().unwrap().take();
    nxcore::nxlog(format!("connect requested (transport={transport})"));

    let (link, hello, phone_ip) = reach_phone(transport, address)?;

    #[cfg(any(windows, target_os = "linux"))]
    {
        start_tunnel(app, state, link, hello, transport, phone_ip)
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let _ = (app, link, phone_ip);
        Err(format!(
            "Phone found: {} — armed and ready. But the tunnel only runs on the Windows and Linux builds of NexLink; this build can only check the phone.",
            hello.phone_name
        ))
    }
}

/// Open the adapter, install the routes, and run the engine on its own
/// thread. The engine is created *inside* that thread: `Engine` holds
/// `Rc` state and is `!Send`, so it can never cross a thread boundary.
///
/// Since v0.3.0 this only returns `Ok` after the engine's connect
/// probes have pushed real traffic through the tunnel (a DNS answer
/// via the phone's interception + a TCP open the phone completed) —
/// "Connected" is verified, never assumed. A failed probe aborts the
/// connect with the probe's named stage error, and the engine thread
/// tears the tunnel down itself.
/// Why Connect cannot proceed without privileges, in the platform's
/// own terms.
#[cfg(windows)]
fn elevation_hint() -> String {
    "NexLink must run as administrator to create the virtual adapter and add routes. Right-click NexLink → Run as administrator, then press Connect again.".to_string()
}

/// See the Windows version.
#[cfg(target_os = "linux")]
fn elevation_hint() -> String {
    "NexLink must run as root to create the TUN device and add routes. Start it with sudo, then press Connect again.".to_string()
}

#[cfg(any(windows, target_os = "linux"))]
fn start_tunnel(
    app: &tauri::AppHandle,
    state: &TunnelState,
    link: TcpStream,
    hello: nxcore::engine::HelloAckInfo,
    transport: &str,
    phone_ip: Option<Ipv4Addr>,
) -> Result<String, String> {
    use adapter::{Device, GATEWAY_ADDRESS};
    use nxcore::engine::{Engine, ProbeStage};
    use nxcore::Ipv4Address;

    if !net_routes::is_elevated() {
        return Err(elevation_hint());
    }

    emit_stage(app, "Opening the virtual adapter…");
    let (device, channels) = Device::open()?;
    emit_stage(app, "Installing routes…");
    let tunnel_routes = match net_routes::install(transport, phone_ip) {
        Ok(r) => r,
        Err(e) => {
            device.stop();
            return Err(e);
        }
    };

    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let last_error = Arc::clone(&state.last_error);
    let summary = format!("{transport} · {}", hello.phone_name);
    let counters = TrafficCounters::new();
    let thread_counters = counters.clone();
    let (probe_tx, probe_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    let stage_app = app.clone();
    let thread = std::thread::Builder::new()
        .name("nexlink-engine".to_string())
        .spawn(move || {
            // The engine lives and dies on this thread.
            let gateway = GATEWAY_ADDRESS.octets();
            let result = (|| -> std::io::Result<()> {
                let mut engine = Engine::new(
                    channels.to_engine_rx,
                    channels.from_engine_tx,
                    link,
                    Ipv4Address::new(gateway[0], gateway[1], gateway[2], gateway[3]),
                    thread_counters,
                )?;
                // The probe gate: real traffic first, "Connected" after.
                let probe = engine.run_probes(&thread_stop, &mut |stage| {
                    emit_stage(
                        &stage_app,
                        match stage {
                            ProbeStage::Dns => "Verifying DNS…",
                            ProbeStage::Traffic => "Verifying traffic…",
                        },
                    );
                });
                let _ = probe_tx.send(probe.clone());
                match probe {
                    Ok(()) => engine.run(thread_stop),
                    Err(e) => Err(std::io::Error::new(std::io::ErrorKind::Other, e)),
                }
            })();
            if let Err(e) = &result {
                *last_error.lock().unwrap() = Some(e.to_string());
            }
            // Teardown, always: routes first so traffic stops entering
            // the adapter, then the adapter itself. (On a clean
            // disconnect the main thread has already set `stop` and is
            // joining us; on a dead link this is what restores the PC's
            // normal routing instead of blackholing it.)
            tunnel_routes.remove();
            device.stop();
        });
    let thread = match thread {
        Ok(t) => t,
        // Practically unreachable (OS thread creation failure); the
        // device and routes are dropped with the closure, so this path
        // leaks a live adapter — noted, not hidden.
        Err(e) => return Err(format!("Could not start the engine thread: {e}")),
    };

    // Wait for the probe gate. On failure the engine thread has
    // recorded the error and is tearing the tunnel down; join it so
    // the routes are gone before the failure is reported.
    match probe_rx.recv_timeout(Duration::from_secs(45)) {
        Ok(Ok(())) => {}
        Ok(Err(stage_err)) => {
            let _ = thread.join();
            return Err(stage_err);
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            // The engine thread died before it could report (its real
            // error is in last_error; it tore the tunnel down itself).
            let _ = thread.join();
            let err = state
                .last_error
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| "the engine stopped during verification".to_string());
            return Err(err);
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            stop.store(true, Ordering::Relaxed);
            let _ = thread.join();
            return Err("Verification timed out — the phone did not answer the tunnel checks. Check the phone's connection and press Connect again.".to_string());
        }
    }

    // USB supervision starts at birth: the forward the tunnel rides on
    // must still be there now that everything else checks out.
    if transport == "usb" && adb_forward_active() == Some(false) {
        stop.store(true, Ordering::Relaxed);
        let _ = thread.join();
        return Err("USB check failed: the adb forward to the phone is not active, so the tunnel has no path to the phone.".to_string());
    }

    nxcore::nxlog(format!("connected via {transport} to {}", hello.phone_name));
    state.inner.lock().unwrap().replace(ActiveTunnel {
        stop,
        thread,
        summary,
        counters,
        transport: transport.to_string(),
    });
    Ok(format!(
        "Connected via {transport} to {} — verified end to end: DNS and traffic are flowing through your phone (adapter 10.77.0.2, gateway 10.77.0.1). Press Disconnect to stop.",
        hello.phone_name
    ))
}

/// The Disconnect button: stop the engine, remove the routes, stop the
/// adapter. Blocking (it joins the engine thread), so it runs off the
/// async runtime.
#[tauri::command]
async fn disconnect(app: tauri::AppHandle) -> Result<String, String> {
    let state = (*app.state::<TunnelState>()).clone();
    tauri::async_runtime::spawn_blocking(move || state.stop_tunnel())
        .await
        .map_err(|e| format!("internal error: {e}"))
}

/// Polled by the window while connected
/// (`connected|<summary>|<bytes down>|<bytes up>`, `dead|<error>`,
/// `disconnected`, `disconnected|<last error>`), so a link that dies
/// on its own shows up in the UI without a click. For USB tunnels the
/// poll also supervises the adb forward (throttled to one check every
/// few seconds): if it disappears under a live tunnel, the tunnel is
/// stopped and the loss is surfaced as `dead|…` instead of the UI
/// showing a false "Connected" forever.
#[tauri::command]
fn tunnel_status(state: tauri::State<TunnelState>) -> String {
    let guard = state.inner.lock().unwrap();
    match guard.as_ref() {
        Some(t) if t.thread.is_finished() => {
            let err = state
                .last_error
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| "the link closed".to_string());
            format!("dead|{err}")
        }
        Some(t) => {
            if t.transport == "usb" {
                let due = {
                    let mut last = state.last_adb_check.lock().unwrap();
                    match *last {
                        Some(at) if at.elapsed() < Duration::from_secs(4) => false,
                        _ => {
                            *last = Some(StdInstant::now());
                            true
                        }
                    }
                };
                if due && adb_forward_active() == Some(false) {
                    let msg = "the adb forward to the phone disappeared — the USB link was lost"
                        .to_string();
                    nxcore::nxlog(format!("connection lost: {msg}"));
                    t.stop.store(true, Ordering::Relaxed);
                    *state.last_error.lock().unwrap() = Some(msg.clone());
                    return format!("dead|{msg}");
                }
            }
            let (down, up) = t.counters.snapshot();
            format!("connected|{}|{}|{}", t.summary, down, up)
        }
        None => match state.last_error.lock().unwrap().clone() {
            Some(e) => format!("disconnected|{e}"),
            None => "disconnected".to_string(),
        },
    }
}

/// The autostart checkbox: is the exe registered to start at login?
/// (HKCU Run key on Windows, XDG autostart entry on Linux; the
/// checkbox errors on other builds.)
#[tauri::command]
fn get_autostart() -> Result<bool, String> {
    #[cfg(windows)]
    {
        autostart::is_enabled()
    }
    #[cfg(target_os = "linux")]
    {
        autostart_linux::is_enabled()
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        Err("Autostart is only available in the Windows and Linux builds of NexLink.".to_string())
    }
}

/// The autostart checkbox: register or unregister the exe.
#[tauri::command]
fn set_autostart(enabled: bool) -> Result<String, String> {
    #[cfg(windows)]
    {
        autostart::set_enabled(enabled)?;
        Ok(if enabled {
            "NexLink will start with Windows. Note: you will see a UAC prompt at each logon — the app needs administrator rights to run."
                .to_string()
        } else {
            "NexLink will no longer start with Windows.".to_string()
        })
    }
    #[cfg(target_os = "linux")]
    {
        autostart_linux::set_enabled(enabled)?;
        Ok(if enabled {
            "NexLink will start at login. Note: connecting still needs root — start the app with sudo before pressing Connect."
                .to_string()
        } else {
            "NexLink will no longer start at login.".to_string()
        })
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let _ = enabled;
        Err("Autostart is only available in the Windows and Linux builds of NexLink.".to_string())
    }
}

fn main() {
    init_logging();
    tauri::Builder::default()
        .manage(TunnelState::new())
        .invoke_handler(tauri::generate_handler![
            connect,
            disconnect,
            tunnel_status,
            get_autostart,
            set_autostart
        ])
        .setup(|app| {
            // Closing the settings window hides it; the app lives in the tray.
            if let Some(window) = app.get_webview_window("settings") {
                let w = window.clone();
                window.on_window_event(move |event| {
                    if let WindowEvent::CloseRequested { api, .. } = event {
                        api.prevent_close();
                        let _ = w.hide();
                    }
                });
            }

            let settings_item = MenuItem::with_id(app, "settings", "Settings", true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "quit", "Exit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&settings_item, &quit_item])?;

            TrayIconBuilder::with_id("nexlink-tray")
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("NexLink")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "settings" => {
                        if let Some(window) = app.get_webview_window("settings") {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        if let Some(window) = tray.app_handle().get_webview_window("settings") {
                            if window.is_visible().unwrap_or(false) {
                                let _ = window.hide();
                            } else {
                                let _ = window.show();
                                let _ = window.set_focus();
                            }
                        }
                    }
                })
                .build(app)?;
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building NexLink")
        .run(|app: &tauri::AppHandle, event: RunEvent| {
            // Quitting (tray menu → Exit) must tear the tunnel down
            // first: stop the engine, remove the routes, stop the
            // adapter — otherwise the PC keeps routing into a dead
            // tunnel (or a stale adapter) after NexLink is gone.
            if let RunEvent::ExitRequested { .. } = event {
                app.state::<TunnelState>().stop_tunnel();
            }
        });
}
