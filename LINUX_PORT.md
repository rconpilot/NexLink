# NexLink — Linux port

Status: **foundation landed** (2026-11-23). The Linux platform layer exists,
compiles, is unit-tested, and the tray app is wired to it. What does *not*
exist yet is a live-tunnel verification or a packaged Linux release — see
"Remaining work". Nothing below claims a working Linux tunnel: this VM has
no `CAP_NET_ADMIN`, so no real TUN device could be created here (the error
path was exercised instead — see "Verification").

## 1. Audit — what was Windows-specific

### `pc/crates/nxcore` (engine + NXLP protocol)
Nothing. The engine is fed packets through `std::sync::mpsc` channels and
is fully platform-agnostic; the only Windows mention was a doc comment.
**No changes needed.**

### `pc/crates/nxpump` (dev harness)
Already Linux-only in practice: it created a TUN device with a raw
`TUNSETIFF` ioctl and configured it with `ip`. But it was not cfg-gated,
so the workspace's `x86_64-pc-windows-gnu` cross-check **failed at
baseline** on this crate.

### `pc/crates/simphone`, `pc/crates/e2e`
Platform-agnostic (the `.windows(4)` hits are slice windows, not the OS).

### `pc/app` (Tauri tray app) — where all the Windows coupling lived
| Piece | Windows mechanism |
|---|---|
| `src/wintun_backend.rs` | `wintun` crate (target-gated dep), `wintun.dll` loaded from resources, adapter addressed via the crate's netsh wrapper, ring-buffer session pumps |
| `src/routes.rs` | shells out to `route` / `netsh`; interface-index binding; route-table + DNS read-back via `netutil` parsers; `is_elevated()` via `net session` |
| `src/autostart.rs` | `reg.exe` HKCU Run key |
| `src/netutil.rs` | pure parsers of `route print` / `netsh` / `reg query` / `adb forward --list` output (cross-platform, unit-tested) |
| `src/main.rs` | log path from `%APPDATA%`; `adb_command()` looked for `adb.exe`; `start_tunnel` was `#[cfg(windows)]` and every other build answered Connect with "the tunnel only runs on the Windows build" |
| resources / config | bundled `wintun.dll`, `adb.exe`, `AdbWinApi.dll`, `AdbWinUsbApi.dll`; `windows-app-manifest.xml` (UAC elevation); `tauri.conf.json` bundle targets = NSIS only |
| tray / window / commands | Tauri 2 — already cross-platform, no changes needed |

## 2. Design — the Linux equivalents

The Windows backend already defined the contract; the Linux layer mirrors
it exactly so the app drives both through one code path:

```
adapter::Device::open() -> Result<(Device, Channels), String>
adapter::Device::stop()
adapter::{ADAPTER_NAME, ADAPTER_ADDRESS, GATEWAY_ADDRESS, ADAPTER_MTU}
net_routes::install(transport, phone_ip) -> Result<TunnelRoutes, String>
net_routes::TunnelRoutes::remove()
net_routes::is_elevated()
```

* **Adapter**: a `/dev/net/tun` device (IFF_TUN | IFF_NO_PI), created
  without IFF_PERSIST so it vanishes when the last handle closes — unlike
  Wintun, no stale adapter is left for the next run. Address/MTU/up via
  `ip` (iproute2), the same shell-out style as the Windows side. Pump
  threads mirror the Wintun ones; the fd is put in non-blocking mode
  because TUN has no `shutdown()` to interrupt a blocking read.
* **Routes**: the same two `/1` device routes (`0.0.0.0/1`,
  `128.0.0.0/1 dev NexLink`) — device routes need no `via` because the
  TUN interface's own /24 makes the gateway on-link — plus the Wi-Fi
  `/32` host route to the phone via the physical gateway (parsed from
  `ip route show default`). Install ends with the v0.3.0-style read-back:
  both /1s must appear in `ip route show` bound to the device.
* **DNS**: systemd-resolved first (`resolvectl dns <if> 10.77.0.1` +
  `resolvectl domain <if> ~.`, reverted with `resolvectl revert`, read
  back after install); fallback is a `/etc/resolv.conf` rewrite with the
  original bytes held in memory and restored verbatim on disconnect.
* **Elevation**: effective uid 0 (parsed from `/proc/self/status`). No
  self-elevation on Linux yet — Connect reports "start it with sudo".
* **Autostart**: XDG autostart entry (`~/.config/autostart/nexlink.desktop`),
  same "points at the running exe" semantics as the Run-key check.
* **Logging**: `$XDG_STATE_HOME/NexLink/nexlink.log` (default
  `~/.local/state/...`), the `%APPDATA%` counterpart.
* **adb**: bundled-copy lookup now uses platform file names (`adb` vs
  `adb.exe`), then PATH as before. No Linux adb is bundled yet.
* **Tauri shell**: carries over unchanged (tray, window, commands are
  Tauri/cross-platform). A `tauri.linux.conf.json` override switches the
  bundle to deb/AppImage, drops the Windows-only resources, and declares
  an `iproute2` deb dependency.

## 3. What was built

* **New crate `pc/crates/nxtun`** (workspace member) — the Linux platform
  layer, `#![cfg(target_os = "linux")]` with target-gated deps so it is an
  empty shell on other targets:
  * `device.rs` — `create_tun()` (raw open + TUNSETIFF, shared with
    nxpump) and `TunDevice` (address/MTU/up + pump threads + `stop()`),
    exporting the same `Device`/`Channels`/constants shape as the Wintun
    backend. Errors are named and honest (EPERM → "needs root or
    CAP_NET_ADMIN — run with sudo"; missing node → check the tun module).
  * `routes.rs` — route/DNS install + verify + remove and `is_elevated`,
    with pure, unit-tested parsers (`ip route`, `resolvectl dns`,
    `/proc/self/status`).
* **nxpump** now uses `nxtun::device::create_tun` (harness and product
  share the ioctl code) and is cfg-gated to a stub off Linux — which also
  fixed the baseline-broken workspace Windows cross-check.
* **Tray app wiring** (`pc/app/src-tauri`):
  * `wintun_backend` gained `Device`/`Channels` aliases; `main.rs` aliases
    `adapter` and `net_routes` per platform and `start_tunnel` is now
    `#[cfg(any(windows, target_os = "linux"))]` — one implementation for
    both. Platform elevation hints, per-platform handshake name
    ("NexLink for Linux"), XDG log path, platform adb file names.
  * New `src/autostart_linux.rs` (XDG entry) wired into the existing
    autostart commands.
  * `Cargo.toml`: `nxtun` as a Linux-only path dependency; new
    `tauri.linux.conf.json` (deb/AppImage, no Windows resources).

## 4. Verification (this VM, 2026-11-23)

| Check | Result |
|---|---|
| `cargo check` (workspace, Linux host) | ✅ pass, no warnings |
| `cargo check --target x86_64-pc-windows-gnu` (workspace) | ✅ pass — **failed at baseline** (ungated nxpump), now green; Windows product code untouched |
| `cargo test -p nxcore -p nxtun` | ✅ 8 + 7 tests pass (incl. ifreq layout, route/DNS/uid parsers) |
| `bash pc/test-e2e.sh` (project CI suite, engine data path) | ✅ E2E PASS |
| TUN error path, live: `nxpump --tun nx0` | ✅ honest failure — `cannot create TUN nx0: Operation not permitted (os error 1) (need root / CAP_NET_ADMIN)`, exit 1, and `ip link show nx0` confirms no device was created. This VM's CapEff has CAP_NET_ADMIN dropped, exactly the sandbox case the code must not fake its way past. |
| App crate `cargo check` (Linux host) | ⚠️ blocked by environment, same as baseline: GTK/WebKit dev libs (`gdk-3.0.pc`) are not installed on this VM, so the build stops in `gdk-sys`'s build script before app code is reached. |
| App crate `cargo check --target x86_64-pc-windows-gnu` | ⚠️ blocked by environment, same as baseline: `x86_64-w64-mingw32-windres` is missing (mingw was wiped by a VM restart), so `tauri-winres` panics in the build script. Not forced, per standing instructions. |

Consequence of the two ⚠️ rows: the app-side wiring is written to mirror
the Windows code path-for-path and was re-read against both backends'
signatures, but it has **not** been compiler-verified yet. The first
Linux CI job (below) is what closes that gap. All genuinely new logic
(TUN backend, Linux routes/DNS) lives in `nxtun`, which *is* fully
checked and tested on this VM.

## 5. Remaining work

1. **Commit** (two commits, in this order — the tree is uncommitted as of
   2026-11-24 morning):
   - *New logo* (approved 2026-11-23): `assets/nexlink-logo*.png`,
     `android/.../mipmap-*/ic_launcher.png`,
     `pc/app/src-tauri/icons/*`. Do **not** include the
     `assets/media-generation-*` byproducts (logo-generation intermediates).
   - *Linux port + CI*: `pc/crates/nxtun/` (new),
     `pc/Cargo.toml` (workspace member), `pc/crates/nxpump/*` (cfg-gating
     + shared `create_tun`), `pc/app/src-tauri/` (`Cargo.toml`,
     `src/main.rs`, `src/wintun_backend.rs`, `src/autostart_linux.rs`,
     `tauri.linux.conf.json`), this file, and
     `.github/workflows/linux.yml` (the Linux CI job).
2. **Linux CI** (`.github/workflows/linux.yml`): on `ubuntu-latest` it
   installs the Tauri Linux system deps, runs `cargo check` for the
   workspace, `cargo test -p nxcore -p nxtun`, `bash pc/test-e2e.sh`,
   and — the step this VM could not run — `cargo check` of the tray-app
   crate on a Linux host, which is what finally compiler-verifies the
   app-side wiring. A second job runs the
   `x86_64-pc-windows-gnu` cross-check so the nxpump gating fix stays
   green. Both jobs must be green before any Linux release is cut.
3. **Real Linux build**: on a Linux machine with the Tauri system deps,
   `cargo tauri build --config tauri.linux.conf.json` (deb + AppImage).
   Not possible on this VM (no GTK/WebKit dev libs, no CAP_NET_ADMIN).
4. **Live tunnel test**: on a real Linux PC + Abol's phone, run with
   sudo, connect, and verify traffic actually flows through the phone
   (same probe-gated checks as Windows v0.3.1). Until this happens,
   **do not claim a working Linux tunnel** — only a compiling,
   unit-tested foundation.
5. **Release**: ship the Linux build only after step 4 passes, and note
   in the release that the new logo lands in the same release.