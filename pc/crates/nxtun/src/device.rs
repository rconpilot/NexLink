//! TUN adapter backend (Linux) — the counterpart of the Windows
//! Wintun backend in the tray app.
//!
//! Creates a `/dev/net/tun` device named after [`crate::ADAPTER_NAME`]
//! at 10.77.0.2/24 (gateway 10.77.0.1) and pumps packets between the
//! device and the two `std::sync::mpsc` channels
//! `nxcore::engine::Engine::new` expects:
//!
//! ```text
//! TUN device ──read──► to_engine_rx ──► Engine (smoltcp stack)
//! TUN device ◄─write── from_engine_tx ◄── Engine
//! ```
//!
//! Requirements on the machine, both surfaced as named errors rather
//! than compile-time concerns:
//! * `/dev/net/tun` must exist and be openable, and creating the
//!   device (`TUNSETIFF`) plus addressing it (via `ip`) needs root or
//!   `CAP_NET_ADMIN`. There is no way around this on Linux — it is
//!   the equivalent of the Windows build's administrator requirement.
//!   Sandboxes that drop `CAP_NET_ADMIN` (including the Muse VM this
//!   was developed on) make `TUNSETIFF` fail with EPERM; `open`
//!   reports that verbatim instead of pretending a device exists.
//! * The `ip` command (iproute2) for address/MTU configuration — the
//!   same shell-out style the Windows side uses for `route`/`netsh`.
//!
//! The device is created without `IFF_PERSIST`, so it disappears when
//! the last file handle closes: `stop()` joins the pump threads, their
//! handles drop, and the interface is gone — no stale adapter is left
//! behind (unlike Windows, where the Wintun adapter persists and is
//! reopened next run).

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::io::AsRawFd;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use crate::{ADAPTER_ADDRESS, ADAPTER_MTU, ADAPTER_NAME};

/// IFF_TUN — a TUN device (IP packets, no ethernet header).
const IFF_TUN: u16 = 0x0001;
/// IFF_NO_PI — no 4-byte packet-information header prefix.
const IFF_NO_PI: u16 = 0x1000;

/// Build the `struct ifreq` prefix `TUNSETIFF` consumes: the 16-byte
/// interface name field followed by the `ifr_flags` u16. Kept separate
/// (and unit-tested) because a wrong offset here fails silently at
/// runtime, not at compile time.
fn build_ifreq(name: &str, flags: u16) -> [u8; 40] {
    let mut ifr = [0u8; 40];
    let name_bytes = name.as_bytes();
    let n = name_bytes.len().min(15); // IFNAMSIZ is 16 including the NUL
    ifr[..n].copy_from_slice(&name_bytes[..n]);
    ifr[16..18].copy_from_slice(&flags.to_ne_bytes());
    ifr
}

/// Open `/dev/net/tun` and claim a TUN device called `name`
/// (`IFF_TUN | IFF_NO_PI`). Returns the device file and the name the
/// kernel assigned (identical to `name` unless `name` was empty).
///
/// This is the low-level half of the backend, shared with the nxpump
/// dev harness. Errors are the real OS errors: ENOENT when the TUN
/// module/device node is missing, EPERM/EACCES without
/// CAP_NET_ADMIN/root.
pub fn create_tun(name: &str) -> io::Result<(File, String)> {
    if name.len() >= 16 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("interface name '{name}' is too long (Linux allows at most 15 bytes)"),
        ));
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/net/tun")?;
    let mut ifr = build_ifreq(name, IFF_TUN | IFF_NO_PI);
    // SAFETY: `file` is a live /dev/net/tun fd; `ifr` is a correctly
    // laid-out ifreq (16-byte name + flags) that outlives the call,
    // exactly what TUNSETIFF expects. The kernel writes the assigned
    // interface name back into the name field.
    let ret = unsafe {
        libc::ioctl(
            file.as_raw_fd(),
            libc::TUNSETIFF,
            ifr.as_mut_ptr() as *mut libc::c_void,
        )
    };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }
    let assigned = {
        let end = ifr[..16].iter().position(|&b| b == 0).unwrap_or(16);
        String::from_utf8_lossy(&ifr[..end]).into_owned()
    };
    Ok((file, assigned))
}

/// Run `ip …`, mapping a spawn failure and a non-zero exit to named
/// errors (the address step needs CAP_NET_ADMIN; the message says so).
fn run_ip(args: &[&str]) -> Result<(), String> {
    let output = Command::new("ip")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("could not run `ip` ({e}) — is iproute2 installed?"))?;
    if output.status.success() {
        Ok(())
    } else {
        let msg = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(if msg.is_empty() {
            format!("`ip {}` failed", args.join(" "))
        } else {
            format!("`ip {}` failed: {msg}", args.join(" "))
        })
    }
}

/// Put the fd into non-blocking mode so the read pump can poll the
/// stop flag. (TUN has no `shutdown()` equivalent; a blocking read on
/// another thread's handle cannot be interrupted by closing it.)
fn set_nonblocking(file: &File) -> io::Result<()> {
    let fd = file.as_raw_fd();
    // SAFETY: plain fcntl flag manipulation on a live fd we own.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// The two channel ends the engine consumes, in exactly the shape
/// `Engine::new(dev_rx, dev_tx, link, gateway)` takes — identical to
/// the Windows backend's channels struct:
/// * `to_engine_rx` — raw IP packets read from the device.
/// * `from_engine_tx` — raw IP packets the engine emits, written to
///   the device.
pub struct TunChannels {
    pub to_engine_rx: Receiver<Vec<u8>>,
    pub from_engine_tx: Sender<Vec<u8>>,
}

/// A live TUN device with its two pump threads.
///
/// The channels live in [`TunChannels`] (returned alongside this
/// handle by [`TunDevice::open`]); this handle owns the threads, and
/// `stop()` ends them. It has no `Drop` impl on purpose: dropping it
/// while the engine still holds the channels would kill the pump
/// mid-tunnel — shutdown is always explicit, via `stop()`.
pub struct TunDevice {
    stop: Arc<AtomicBool>,
    handles: Vec<JoinHandle<()>>,
}

impl TunDevice {
    /// Create the TUN device, address it 10.77.0.2/24, set the shared
    /// MTU, bring it up, and spawn the two pump threads.
    pub fn open() -> Result<(TunDevice, TunChannels), String> {
        let (file, assigned) = create_tun(ADAPTER_NAME).map_err(|e| {
            if e.kind() == io::ErrorKind::PermissionDenied {
                format!(
                    "Could not create the '{ADAPTER_NAME}' TUN device ({e}). \
                     Creating it needs root or CAP_NET_ADMIN — run NexLink with sudo."
                )
            } else {
                format!(
                    "Could not create the '{ADAPTER_NAME}' TUN device ({e}). \
                     Is the tun kernel module loaded (/dev/net/tun present)?"
                )
            }
        })?;
        debug_assert_eq!(assigned, ADAPTER_NAME);

        // Address + MTU + up, via iproute2 (needs the same privilege
        // as TUNSETIFF; the app checks elevation before calling open,
        // but the error is named here too in case that check raced).
        let configure = (|| -> Result<(), String> {
            run_ip(&[
                "addr",
                "add",
                &format!("{ADAPTER_ADDRESS}/24"),
                "dev",
                ADAPTER_NAME,
            ])?;
            run_ip(&[
                "link",
                "set",
                "dev",
                ADAPTER_NAME,
                "mtu",
                &ADAPTER_MTU.to_string(),
            ])?;
            run_ip(&["link", "set", "dev", ADAPTER_NAME, "up"])?;
            Ok(())
        })();
        if let Err(e) = configure {
            // The device itself vanishes when `file` drops (no
            // IFF_PERSIST), so there is nothing to clean up by hand.
            return Err(format!(
                "Could not configure the '{ADAPTER_NAME}' device: {e}"
            ));
        }

        set_nonblocking(&file)
            .map_err(|e| format!("Could not configure the '{ADAPTER_NAME}' device ({e})."))?;

        let (to_engine_tx, to_engine_rx) = channel::<Vec<u8>>();
        let (from_engine_tx, from_engine_rx) = channel::<Vec<u8>>();
        let stop = Arc::new(AtomicBool::new(false));

        // Device -> engine. The fd is non-blocking, so WouldBlock just
        // means "no packet yet": nap briefly and re-check the stop
        // flag. A read of 0 or a hard error means the device is gone.
        let mut reader = file
            .try_clone()
            .map_err(|e| format!("Could not clone the TUN handle ({e})."))?;
        let read_stop = Arc::clone(&stop);
        let read_handle = std::thread::spawn(move || {
            let mut buf = vec![0u8; 2048];
            loop {
                if read_stop.load(Ordering::Relaxed) {
                    return;
                }
                match reader.read(&mut buf) {
                    Ok(0) => return,
                    Ok(n) => {
                        // The engine end is gone: nothing left to pump to.
                        if to_engine_tx.send(buf[..n].to_vec()).is_err() {
                            return;
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => return,
                }
            }
        });

        // Engine -> device. `recv_timeout` lets the thread notice
        // `stop` even while the engine holds the sender open. Writes
        // retry briefly on WouldBlock (a full device queue) but never
        // spin forever: a wedged device must not pin the thread past
        // `stop()`.
        let mut writer = file;
        let write_stop = Arc::clone(&stop);
        let write_handle = std::thread::spawn(move || loop {
            if write_stop.load(Ordering::Relaxed) {
                return;
            }
            match from_engine_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(pkt) => {
                    let mut rest = &pkt[..];
                    while !rest.is_empty() {
                        match writer.write(rest) {
                            Ok(0) => return,
                            Ok(n) => rest = &rest[n..],
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                                if write_stop.load(Ordering::Relaxed) {
                                    return;
                                }
                                std::thread::sleep(Duration::from_millis(1));
                            }
                            Err(_) => return,
                        }
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
            }
        });

        let device = TunDevice {
            stop,
            handles: vec![read_handle, write_handle],
        };
        let channels = TunChannels {
            to_engine_rx,
            from_engine_tx,
        };
        Ok((device, channels))
    }

    /// Stop the pump threads. Their device handles drop with them, and
    /// because the device was created without `IFF_PERSIST` the
    /// interface itself is removed by the kernel — nothing is left
    /// behind for the next run to trip over.
    pub fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        for handle in self.handles {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ifreq_layout() {
        let ifr = build_ifreq("NexLink", IFF_TUN | IFF_NO_PI);
        assert_eq!(&ifr[..7], b"NexLink");
        assert!(ifr[7..16].iter().all(|&b| b == 0));
        assert_eq!(u16::from_ne_bytes([ifr[16], ifr[17]]), 0x1001);
        assert!(ifr[18..].iter().all(|&b| b == 0));
    }

    #[test]
    fn ifreq_truncates_long_names() {
        // IFNAMSIZ is 16 including the NUL; a longer name must never
        // bleed into the flags field.
        let ifr = build_ifreq("a-very-long-interface-name", IFF_TUN);
        assert_eq!(&ifr[..15], b"a-very-long-int");
        assert_eq!(ifr[15], 0);
        assert_eq!(u16::from_ne_bytes([ifr[16], ifr[17]]), IFF_TUN);
    }

    #[test]
    fn overlong_name_is_rejected() {
        // 16+ bytes can never be a valid Linux interface name.
        assert!(create_tun("0123456789abcdef").is_err());
    }
}
