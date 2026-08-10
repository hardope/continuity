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
    let addr = *info.get_addresses().iter().next()?;
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
