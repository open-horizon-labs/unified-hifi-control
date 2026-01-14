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
    let service_name = format!("{}.{}", name.replace(' ', "-"), service_type);

    let service_info = ServiceInfo::new(
        service_type,
        name,
        &gethostname::gethostname().to_string_lossy(),
        (), // All local addresses
        port,
        Some(txt),
    )?;

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
