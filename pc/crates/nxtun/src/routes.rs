//! Linux route + DNS management for the tunnel — the counterpart of
//! the Windows `routes.rs` in the tray app, with the same contract:
//! [`install`] returns a [`TunnelRoutes`] whose [`TunnelRoutes::remove`]
//! undoes exactly what was installed.
//!
//! While connected, NexLink installs two `/1` routes via the TUN
//! device (`0.0.0.0/1` + `128.0.0.0/1`, which together beat the default
//! route by longest-prefix match without deleting it), plus — on
//! Wi-Fi — a `/32` host route to the phone via the physical gateway,
//! so the link stream itself never enters the tunnel. DNS is pointed
//! at the virtual gateway (10.77.0.1) so the phone's DNS interception
//! answers every lookup.
//!
//! All of it goes through the `ip` command (iproute2) — there is no
//! stable Rust API for this — and needs root / CAP_NET_ADMIN; see
//! [`is_elevated`].
//!
//! DNS has two mechanisms, tried in order:
//! 1. **systemd-resolved** (`resolvectl dns <if> 10.77.0.1` +
//!    `resolvectl domain <if> ~.`): per-link DNS, cleanly reverted
//!    with `resolvectl revert`. This is the normal desktop case.
//! 2. **`/etc/resolv.conf` rewrite**: the original file is kept in
//!    memory and restored verbatim on disconnect. Cruder — another
//!    network manager can fight over the file while connected — but
//!    it works on systems without resolved.
//!
//! Like the Windows side since v0.3.0, installation ends with a
//! read-back: the live route table must show both /1s on the NexLink
//! device (and, on the resolvectl path, the link's DNS must read back
//! as 10.77.0.1). Success is what the system reports, not what the
//! commands claimed.

use std::net::Ipv4Addr;
use std::process::{Command, Stdio};

use crate::{ADAPTER_NAME, GATEWAY_ADDRESS};

/// Run a command, capturing its output; success is the exit code only.
fn run(cmd: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new(cmd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("could not run {cmd} ({e})"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        let mut msg = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if msg.is_empty() {
            msg = String::from_utf8_lossy(&output.stdout).trim().to_string();
        }
        Err(if msg.is_empty() {
            format!("{cmd} failed")
        } else {
            format!("{cmd} failed: {msg}")
        })
    }
}

// ---------------------------------------------------------------------------
// Pure parsers (unit-tested)
// ---------------------------------------------------------------------------

/// The default gateway from `ip route show default` output, e.g.
/// `default via 192.168.1.1 dev wlan0 proto dhcp metric 600`.
pub fn parse_default_gateway(ip_route_out: &str) -> Option<Ipv4Addr> {
    for line in ip_route_out.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.first() == Some(&"default") {
            if let Some(pos) = fields.iter().position(|f| *f == "via") {
                if let Some(ip) = fields.get(pos + 1) {
                    if let Ok(addr) = ip.parse() {
                        return Some(addr);
                    }
                }
            }
        }
    }
    None
}

/// Does `ip route show` output contain `dest` routed via `dev`?
/// Matches lines like `10.77.0.0/24 dev NexLink proto kernel scope
/// link src 10.77.0.2` and `0.0.0.0/1 dev NexLink scope link`. A bare
/// `0.0.0.0/1` may be printed as `0.0.0.0/1` exactly; the caller passes
/// the canonical prefix string.
pub fn route_on_device(ip_route_out: &str, prefix: &str, dev: &str) -> bool {
    ip_route_out.lines().any(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.first() != Some(&prefix) {
            return false;
        }
        fields
            .windows(2)
            .any(|pair| pair[0] == "dev" && pair[1] == dev)
    })
}

/// Does `resolvectl dns <if>` output list `server` for the link?
/// Output shape: `Link 7 (NexLink): 10.77.0.1` (possibly several
/// addresses, possibly wrapped across lines on old versions — a
/// substring check on the address is the robust read-back here).
pub fn resolvectl_dns_listed(resolvectl_out: &str, server: Ipv4Addr) -> bool {
    resolvectl_out.contains(&server.to_string())
}

/// The effective uid from `/proc/self/status` content
/// (`Uid:\t<real>\t<effective>\t<saved>\t<fs>`).
pub fn effective_uid(proc_status: &str) -> Option<u32> {
    for line in proc_status.lines() {
        if let Some(rest) = line.strip_prefix("Uid:") {
            let mut fields = rest.split_whitespace();
            let _real = fields.next()?;
            return fields.next()?.parse().ok();
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Installed state
// ---------------------------------------------------------------------------

/// One route we installed, so disconnect can remove exactly it.
struct AddedRoute {
    /// Canonical prefix as `ip route` prints it, e.g. `0.0.0.0/1` or
    /// `192.168.1.55/32` — also the delete key.
    prefix: String,
    /// `via <gateway>` for the Wi-Fi host route; `None` for the /1s,
    /// which are device routes on the TUN interface.
    via: Option<Ipv4Addr>,
    dev: Option<String>,
}

impl AddedRoute {
    fn add(&self) -> Result<(), String> {
        let mut args: Vec<String> = vec!["route".into(), "add".into(), self.prefix.clone()];
        if let Some(gw) = self.via {
            args.push("via".into());
            args.push(gw.to_string());
        }
        if let Some(dev) = &self.dev {
            args.push("dev".into());
            args.push(dev.clone());
        }
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        run("ip", &refs).map(|_| ())
    }

    /// Best effort: a missing route is not an error on the way out.
    fn delete(&self) {
        let mut args: Vec<String> = vec!["route".into(), "del".into(), self.prefix.clone()];
        if let Some(gw) = self.via {
            args.push("via".into());
            args.push(gw.to_string());
        }
        if let Some(dev) = &self.dev {
            args.push("dev".into());
            args.push(dev.clone());
        }
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let _ = run("ip", &refs);
    }
}

/// How DNS was pointed at the gateway, so removal undoes it the
/// matching way.
enum DnsChange {
    /// `resolvectl revert <if>` restores the link's DNS.
    Resolvectl,
    /// The original /etc/resolv.conf contents, restored verbatim.
    ResolvConf(Vec<u8>),
}

/// The routes + DNS change installed by [`install`]. Dropping it does
/// nothing on purpose — removal is always explicit, via
/// [`TunnelRoutes::remove`], so a panic mid-connect cannot silently
/// leave the PC routed nowhere.
pub struct TunnelRoutes {
    routes: Vec<AddedRoute>,
    dns: Option<DnsChange>,
}

impl TunnelRoutes {
    /// Remove every route installed by [`install`] and restore DNS.
    /// Best effort throughout: disconnect must never fail because one
    /// piece is already gone.
    pub fn remove(self) {
        for route in &self.routes {
            route.delete();
        }
        match self.dns {
            Some(DnsChange::Resolvectl) => {
                let _ = run("resolvectl", &["revert", ADAPTER_NAME]);
            }
            Some(DnsChange::ResolvConf(original)) => {
                let _ = std::fs::write("/etc/resolv.conf", original);
            }
            None => {}
        }
    }
}

/// The PC's current default gateway (the `default via …` route).
/// Needed for the Wi-Fi `/32` host route: the link to the phone must
/// keep going via the physical network, not the tunnel.
pub fn physical_gateway() -> Result<Ipv4Addr, String> {
    let out = run("ip", &["route", "show", "default"])?;
    parse_default_gateway(&out).ok_or_else(|| {
        "Could not find the physical default gateway (needed for the Wi-Fi host route). Is this PC connected to a network?"
            .to_string()
    })
}

/// Is systemd-resolved managing DNS? `resolvectl status` exits 0
/// only when resolved is up and reachable over D-Bus.
fn resolved_available() -> bool {
    Command::new("resolvectl")
        .args(["status"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Point the tunnel link's DNS at the virtual gateway, by whichever
/// mechanism this system has. On the resolvectl path the `~.` routing
/// domain sends *all* lookups to the link's resolver (without it,
/// resolved would only use the link for names under a search domain).
fn install_dns() -> Result<DnsChange, String> {
    if resolved_available() {
        run(
            "resolvectl",
            &["dns", ADAPTER_NAME, &GATEWAY_ADDRESS.to_string()],
        )
        .map_err(|e| format!("Could not set the tunnel DNS to {GATEWAY_ADDRESS}: {e}"))?;
        if let Err(e) = run("resolvectl", &["domain", ADAPTER_NAME, "~."]) {
            let _ = run("resolvectl", &["revert", ADAPTER_NAME]);
            return Err(format!("Could not set the tunnel DNS routing domain: {e}"));
        }
        return Ok(DnsChange::Resolvectl);
    }
    // Fallback: rewrite /etc/resolv.conf, keeping the original bytes
    // for an exact restore on disconnect.
    let original = std::fs::read("/etc/resolv.conf")
        .map_err(|e| format!("Could not read /etc/resolv.conf ({e})."))?;
    std::fs::write(
        "/etc/resolv.conf",
        format!("nameserver {GATEWAY_ADDRESS}\n"),
    )
    .map_err(|e| format!("Could not write /etc/resolv.conf ({e})."))?;
    Ok(DnsChange::ResolvConf(original))
}

/// Read back what `install` just did and prove it stuck: both /1
/// routes on the NexLink device in the live route table, and — on the
/// resolvectl path — the link DNS reading back as the gateway. (The
/// resolv.conf path has nothing to read back beyond the file we just
/// wrote; the routes are the real proof there.)
fn verify_install(dns: &DnsChange) -> Result<(), String> {
    let table = run("ip", &["route", "show"])?;
    for prefix in ["0.0.0.0/1", "128.0.0.0/1"] {
        if !route_on_device(&table, prefix, ADAPTER_NAME) {
            return Err(format!(
                "Route check failed: {prefix} is not routed via the {ADAPTER_NAME} device in the route table"
            ));
        }
    }
    if let DnsChange::Resolvectl = dns {
        let out = run("resolvectl", &["dns", ADAPTER_NAME])?;
        if !resolvectl_dns_listed(&out, GATEWAY_ADDRESS) {
            return Err(format!(
                "DNS check failed: the {ADAPTER_NAME} link's DNS server is not {GATEWAY_ADDRESS} after setting it"
            ));
        }
    }
    Ok(())
}

/// Install the tunnel routes (and DNS), then verify them by reading
/// the system's state back. On any failure the changes made so far
/// are removed again and the error is returned.
pub fn install(transport: &str, phone_ip: Option<Ipv4Addr>) -> Result<TunnelRoutes, String> {
    let mut installed = TunnelRoutes {
        routes: Vec::new(),
        dns: None,
    };

    // Wi-Fi first: without the /32 host route, the link stream to the
    // phone would itself be routed into the tunnel and deadlock.
    if transport == "wifi" {
        let ip =
            phone_ip.ok_or_else(|| "Wi-Fi connect needs the phone's IP address.".to_string())?;
        let gw = physical_gateway()?;
        let host = AddedRoute {
            prefix: format!("{ip}/32"),
            via: Some(gw),
            dev: None,
        };
        // A crashed run may have left this route behind; clear it first.
        host.delete();
        host.add().map_err(|e| {
            format!("Could not add the host route to the phone ({ip} via {gw}): {e}")
        })?;
        installed.routes.push(host);
    }

    // The two /1s together cover the whole IPv4 space and beat the
    // default route (longest-prefix match) without deleting it. They
    // are device routes: the TUN interface's own /24 makes the
    // gateway on-link, so no `via` is needed.
    for prefix in ["0.0.0.0/1", "128.0.0.0/1"] {
        let route = AddedRoute {
            prefix: prefix.to_string(),
            via: None,
            dev: Some(ADAPTER_NAME.to_string()),
        };
        route.delete(); // stale route from a crashed run
        if let Err(e) = route.add() {
            installed.remove();
            return Err(format!(
                "Could not add route {prefix} dev {ADAPTER_NAME}: {e}"
            ));
        }
        installed.routes.push(route);
    }

    // Point DNS at the virtual gateway so lookups go through the
    // tunnel and the phone's DNS interception answers them.
    match install_dns() {
        Ok(change) => installed.dns = Some(change),
        Err(e) => {
            installed.remove();
            return Err(e);
        }
    }

    // Prove it: read the route table (and link DNS) back.
    if let Err(e) = verify_install(installed.dns.as_ref().expect("dns just installed")) {
        installed.remove();
        return Err(e);
    }

    Ok(installed)
}

/// Is this process running with an effective uid of 0 (root)? Route
/// and device changes need it (or CAP_NET_ADMIN, which in practice
/// means a root-owned helper). Used for a clear error up front
/// instead of a cryptic `ip` failure.
pub fn is_elevated() -> bool {
    let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
        return false;
    };
    effective_uid(&status) == Some(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_gateway_parsing() {
        let out = "default via 192.168.1.1 dev wlan0 proto dhcp metric 600\n\
                   192.168.1.0/24 dev wlan0 proto kernel scope link src 192.168.1.55 metric 600\n";
        assert_eq!(
            parse_default_gateway(out),
            Some(Ipv4Addr::new(192, 168, 1, 1))
        );
        // A default route without a gateway (point-to-point) yields none.
        assert_eq!(
            parse_default_gateway("default dev ppp0 proto static\n"),
            None
        );
        assert_eq!(parse_default_gateway(""), None);
    }

    #[test]
    fn route_on_device_matching() {
        let out = "0.0.0.0/1 dev NexLink scope link\n\
                   128.0.0.0/1 dev NexLink scope link\n\
                   default via 192.168.1.1 dev wlan0 proto dhcp metric 600\n";
        assert!(route_on_device(out, "0.0.0.0/1", "NexLink"));
        assert!(route_on_device(out, "128.0.0.0/1", "NexLink"));
        // Same prefix, wrong device: not a match.
        assert!(!route_on_device(out, "0.0.0.0/1", "wlan0"));
        // Absent prefix: not a match.
        assert!(!route_on_device(out, "10.0.0.0/8", "NexLink"));
    }

    #[test]
    fn resolvectl_dns_readback() {
        assert!(resolvectl_dns_listed(
            "Link 7 (NexLink): 10.77.0.1",
            GATEWAY_ADDRESS
        ));
        assert!(!resolvectl_dns_listed(
            "Link 7 (NexLink): 192.168.1.1",
            GATEWAY_ADDRESS
        ));
    }

    #[test]
    fn effective_uid_parsing() {
        let status = "Name:\tnexlink\nUid:\t1000\t0\t0\t0\nGid:\t1000\t1000\t1000\t1000\n";
        assert_eq!(effective_uid(status), Some(0));
        let status = "Uid:\t1000\t1000\t1000\t1000\n";
        assert_eq!(effective_uid(status), Some(1000));
        assert_eq!(effective_uid("nothing here"), None);
    }
}
