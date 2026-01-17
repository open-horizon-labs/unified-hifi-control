//! mDNS service advertising for knob discovery
//!
//! Publishes a _roonknob._tcp service so S3 Knob devices can discover the server.

use mdns_sd::{ServiceDaemon, ServiceInfo};
use std::collections::HashMap;

/// Advertise the service via mDNS
pub fn advertise(port: u16, name: &str, base_url: &str) -> anyhow::Result<ServiceDaemon> {
    let mdns = ServiceDaemon::new()?;

    // Build TXT records
    let mut txt = HashMap::new();
    txt.insert("base".to_string(), base_url.to_string());
    txt.insert("api".to_string(), "1".to_string());

    // Create service info
    // Type is "_roonknob._tcp.local."
    let service_type = "_roonknob._tcp.local.";

    // Get hostname and ensure it ends with ".local." for mdns_sd
    let raw_hostname = gethostname::gethostname().to_string_lossy().to_string();
    let hostname = if raw_hostname.ends_with(".local.") {
        raw_hostname
    } else if raw_hostname.ends_with(".local") {
        format!("{}.", raw_hostname)
    } else {
        format!("{}.local.", raw_hostname)
    };

    let service_info = ServiceInfo::new(
        service_type,
        name,
        &hostname,
        (), // Will be filled by enable_addr_auto()
        port,
        Some(txt),
    )?
    .enable_addr_auto();

    tracing::info!(
        "mDNS: Publishing service '{}' on port {} (type: {})",
        name,
        port,
        service_type
    );

    // Register the service
    mdns.register(service_info)?;

    tracing::info!("mDNS: Service registered successfully");

    Ok(mdns)
}
