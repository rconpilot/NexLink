//! NexLink Linux platform layer — the counterpart of the Windows
//! Wintun backend + `route`/`netsh` management in the tray app.
//!
//! * [`device`] — a `/dev/net/tun` adapter backend exposing exactly the
//!   shape the Windows backend does (`Device::open() -> (Device,
//!   Channels)`, `Device::stop()`), so the tray app's tunnel code is
//!   written once against either.
//! * [`routes`] — the Linux route + DNS management: the two `/1`
//!   routes via the TUN device, the Wi-Fi `/32` host route to the
//!   phone, and DNS pointed at the virtual gateway (via
//!   systemd-resolved when present, `/etc/resolv.conf` otherwise),
//!   with the same install-then-read-back verification the Windows
//!   side learned in v0.3.0.
//!
//! The crate is empty on non-Linux targets (everything is cfg-gated),
//! so workspace cross-checks for Windows are unaffected.

#![cfg(target_os = "linux")]

pub mod device;
pub mod routes;

use std::net::Ipv4Addr;

/// Name of the TUN interface as it appears in `ip link`. Mirrors the
/// Windows adapter name; Linux interface names are limited to 15
/// bytes, which this fits comfortably.
pub const ADAPTER_NAME: &str = "NexLink";
/// Address of this end of the tunnel (the PC side).
pub const ADAPTER_ADDRESS: Ipv4Addr = Ipv4Addr::new(10, 77, 0, 2);
/// Netmask for the /24 tunnel network.
pub const ADAPTER_NETMASK: Ipv4Addr = Ipv4Addr::new(255, 255, 255, 0);
/// The phone side of the tunnel. The engine also uses it as its
/// default route ("routed locally" — see `nxcore::engine::Engine::new`).
pub const GATEWAY_ADDRESS: Ipv4Addr = Ipv4Addr::new(10, 77, 0, 1);
/// MTU of the TUN device — the shared link MTU from nxcore, so the
/// device and the engine's stack can never disagree (the 1500/1400
/// split was the v0.2.1 no-traffic bug on Windows).
pub const ADAPTER_MTU: usize = nxcore::LINK_MTU;

/// The device handle + channels, under the same names the Windows
/// backend exports, so the app can alias either platform's module as
/// `adapter` and use one code path.
pub use device::{TunChannels as Channels, TunDevice as Device};
