//! mDNS service advertising for knob discovery
//!
//! Publishes the legacy `_roonknob._tcp` service and the dedicated
//! `_uhc._tcp` service so native companions can discover a UHC server.

use mdns_sd::{ServiceDaemon, ServiceInfo};
use std::collections::HashMap;

pub const ROON_KNOB_SERVICE_TYPE: &str = "_roonknob._tcp.local.";
pub const UHC_SERVICE_TYPE: &str = "_uhc._tcp.local.";

/// Advertise the service via mDNS
pub fn advertise(port: u16, name: &str, base_url: &str) -> anyhow::Result<ServiceDaemon> {
    let mdns = ServiceDaemon::new()?;

    // Build TXT records
    let mut txt = HashMap::new();
    txt.insert("base".to_string(), base_url.to_string());
    txt.insert("api".to_string(), "1".to_string());

    // Get hostname and ensure it ends with ".local." for mdns_sd
    let raw_hostname = gethostname::gethostname().to_string_lossy().to_string();
    let hostname = if raw_hostname.ends_with(".local.") {
        raw_hostname
    } else if raw_hostname.ends_with(".local") {
        format!("{}.", raw_hostname)
    } else {
        format!("{}.local.", raw_hostname)
    };

    for (service_type, service_name, mut service_txt) in [
        (ROON_KNOB_SERVICE_TYPE, name.to_string(), txt.clone()),
        (UHC_SERVICE_TYPE, format!("{name} UHC"), {
            let mut txt = txt.clone();
            txt.insert("service".to_string(), "uhc".to_string());
            txt
        }),
    ] {
        let service_info = ServiceInfo::new(
            service_type,
            &service_name,
            &hostname,
            (), // Will be filled by enable_addr_auto()
            port,
            Some(std::mem::take(&mut service_txt)),
        )?
        .enable_addr_auto();
        tracing::info!(
            "mDNS: Publishing service '{}' on port {} (type: {})",
            service_name,
            port,
            service_type
        );
        mdns.register(service_info)?;
    }

    tracing::info!("mDNS: UHC and legacy knob services registered successfully");

    Ok(mdns)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publishes_distinct_companion_service_type() {
        assert_ne!(UHC_SERVICE_TYPE, ROON_KNOB_SERVICE_TYPE);
        assert_eq!(UHC_SERVICE_TYPE, "_uhc._tcp.local.");
    }
}
