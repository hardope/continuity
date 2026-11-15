use continuity_proto::{DeviceInfo, Platform};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use std::net::SocketAddr;

pub const SERVICE_TYPE: &str = "_continuity._tcp.local.";

#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    #[error("mdns error: {0}")]
    Mdns(#[from] mdns_sd::Error),
}

#[derive(Debug, Clone)]
pub struct DiscoveredPeer {
    pub device: DeviceInfo,
    pub addr: SocketAddr,
}

/// Wraps mDNS/DNS-SD advertise + browse for the `_continuity._tcp.local`
/// service. One `Discovery` instance per process is enough — `advertise`
/// and `browse` can both be called on it.
///
/// **There is deliberately no "refresh" method here.** `ServiceDaemon::
/// browse()` registers exactly one listener per service type — call it a
/// second time for the same type and the *first* listener is silently
/// overwritten (straight from `mdns_sd`'s own doc comment: "if there is
/// already a listener, it will be updated, i.e. overwritten"). A real bug
/// shipped because of this: `Discovery` used to derive `Clone` specifically
/// so a second handle could call `browse()` again as a "refresh", which
/// orphaned the original receiver the whole engine's browse loop actually
/// reads from — killing all further discovery, new devices and
/// rediscovery-triggered reconnects alike, after the first refresh. `mdns_sd`
/// already re-queries on its own over time via retransmission, using the
/// original listener, so nothing external is needed for that. If a manual
/// "refresh" is ever wanted again, it needs `stop_browse` + `browse` and a
/// way to hand the *new* receiver back to whatever's reading the old one —
/// not a second call on a cloned handle.
pub struct Discovery {
    daemon: ServiceDaemon,
}

impl Discovery {
    pub fn new() -> Result<Self, DiscoveryError> {
        Ok(Self {
            daemon: ServiceDaemon::new()?,
        })
    }

    /// Announces this device on the LAN. `port` is the TLS listener port
    /// from `connection::listen`.
    pub fn advertise(&self, device: &DeviceInfo, port: u16) -> Result<(), DiscoveryError> {
        // DNS labels are capped at 63 bytes, so the full 64-char hex device
        // id can't be used directly — a 12-char prefix is still 48 bits of
        // collision resistance, plenty for a LAN's worth of devices.
        let short_id = &device.id[..12];
        let host_name = format!("{short_id}.local.");
        let instance_name = short_id.to_string();

        let properties = [
            ("id", device.id.as_str()),
            ("name", device.name.as_str()),
            ("platform", platform_str(device.platform)),
            ("protocol_version", &device.protocol_version.to_string()),
        ];

        let service = ServiceInfo::new(
            SERVICE_TYPE,
            &instance_name,
            &host_name,
            "",
            port,
            &properties[..],
        )?
        .enable_addr_auto();

        self.daemon.register(service)?;
        Ok(())
    }

    /// Starts browsing; each event is a peer appearing/updating/leaving.
    /// Callers filter/collect these into their own view of the mesh.
    pub fn browse(&self) -> Result<mdns_sd::Receiver<ServiceEvent>, DiscoveryError> {
        Ok(self.daemon.browse(SERVICE_TYPE)?)
    }

    pub fn shutdown(&self) -> Result<(), DiscoveryError> {
        self.daemon.shutdown()?;
        Ok(())
    }
}

fn platform_str(platform: Platform) -> &'static str {
    match platform {
        Platform::MacOs => "mac_os",
        Platform::Windows => "windows",
        Platform::Linux => "linux",
        Platform::Android => "android",
        Platform::Ios => "ios",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_with_only_a_link_local_ipv6_address_has_no_address_yet() {
        let make = |addrs: &[&str]| {
            let props = [("id", "abc"), ("name", "A"), ("platform", "linux"), ("protocol_version", "4")];
            let info = ServiceInfo::new(SERVICE_TYPE, "abc", "abc.local.", addrs.join(","), 1234, &props[..]).unwrap();
            peer_from_service_info(&info).map(|p| p.addr.ip())
        };
        assert_eq!(make(&["fe80::1"]), None, "link-local only: wait for a better resolve");
        assert_eq!(make(&["fe80::1", "192.168.1.5"]).map(|a| a.to_string()), Some("192.168.1.5".to_string()));
        assert_eq!(make(&["fe80::1", "2001:db8::5"]).map(|a| a.to_string()), Some("2001:db8::5".to_string()), "a routable IPv6 address is fine");
    }
}

fn platform_from_str(s: &str) -> Option<Platform> {
    match s {
        "mac_os" => Some(Platform::MacOs),
        "windows" => Some(Platform::Windows),
        "linux" => Some(Platform::Linux),
        "android" => Some(Platform::Android),
        "ios" => Some(Platform::Ios),
        _ => None,
    }
}

/// Extracts a `DiscoveredPeer` from a resolved `ServiceInfo`. Returns
/// `None` if the TXT record is missing an expected field or has no
/// resolved address yet (mdns-sd sometimes delivers the name before the
/// address record).
pub fn peer_from_service_info(info: &ServiceInfo) -> Option<DiscoveredPeer> {
    let props = info.get_properties();
    let id = props.get_property_val_str("id")?.to_string();
    let name = props.get_property_val_str("name")?.to_string();
    let platform = platform_from_str(props.get_property_val_str("platform")?)?;
    let protocol_version = props.get_property_val_str("protocol_version")?.parse().ok()?;
    // `get_addresses()` is an unordered `HashSet` mixing the real LAN IPv4
    // address with link-local IPv6 ones (`fe80::...`) that carry no
    // interface/zone index the way they're resolved here — dialing one of
    // those fails outright ("no route to host"), and plain `.next()` could
    // land on either. Prefer IPv4 (always safely dialable on the LAN);
    // fall back to whatever's there for IPv6-only networks.
    // A link-local IPv6 address (`fe80::/10`) is never dialable from here
    // — without a scope id `connect` fails with "invalid argument" (it did,
    // on CI) — so a record that resolved only to those has no address yet:
    // mdns-sd delivers the IPv4 one in a later event.
    let addresses = info.get_addresses();
    let dialable = |a: &&std::net::IpAddr| match a {
        std::net::IpAddr::V4(_) => true,
        std::net::IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) != 0xfe80,
    };
    let addr = addresses.iter().filter(dialable).find(|a| a.is_ipv4()).or_else(|| addresses.iter().find(dialable)).copied()?;
    let port = info.get_port();

    Some(DiscoveredPeer {
        device: DeviceInfo {
            id,
            name,
            platform,
            protocol_version,
        },
        addr: SocketAddr::new(addr, port),
    })
}
