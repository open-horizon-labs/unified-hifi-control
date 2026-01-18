//! LMS (Logitech Media Server) Discovery via UDP Broadcast
//!
//! Implements server discovery using UDP broadcast on port 3483.
//! Protocol: Send TLV request, receive TLV response with server info.
//!
//! Reference: https://github.com/LMS-Community/slimserver/blob/776e969ec5f8101f20f7687f525d42674ea52900/Slim/Networking/Discovery.pm#L108
//!
//! Example usage with socat:
//! ```bash
//! echo -ne "eIPAD\x00NAME\x00JSON\x00UUID\x00VERS\x00" | socat -t5 - udp-datagram:255.255.255.255:3483,broadcast | od -Ax -bc
//! ```

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::time::timeout;

/// LMS discovery port (standard for Squeezebox protocol)
pub const LMS_DISCOVERY_PORT: u16 = 3483;

/// Default discovery timeout in milliseconds
const LMS_DISCOVERY_TIMEOUT_MS: u64 = 3000;

/// Discovery request packet
/// Format: TLV (Type-Length-Value) with null-terminated type names
/// Types requested: IPAD, NAME, JSON, UUID, VERS
const LMS_DISCOVERY_REQUEST: &[u8] = b"eIPAD\x00NAME\x00JSON\x00UUID\x00VERS\x00";

/// Discovered LMS server information
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DiscoveredLms {
    /// Server IP address (from IPAD field or response source)
    pub host: String,
    /// JSON-RPC port (from JSON field, typically 9000)
    pub json_port: u16,
    /// Server/library name (from NAME field)
    pub name: String,
    /// Server UUID (from UUID field)
    pub uuid: String,
    /// Server version (from VERS field, may be unreliable)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// TLV response field types
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum TlvType {
    Ipad,
    Name,
    Json,
    Uuid,
    Vers,
    Unknown,
}

impl TlvType {
    fn from_tag(tag: &[u8]) -> Self {
        match tag {
            b"IPAD" => TlvType::Ipad,
            b"NAME" => TlvType::Name,
            b"JSON" => TlvType::Json,
            b"UUID" => TlvType::Uuid,
            b"VERS" => TlvType::Vers,
            _ => TlvType::Unknown,
        }
    }
}

/// Parse TLV (Type-Length-Value) response from LMS discovery
///
/// Format:
/// - First byte: 'E' or 'e' (response marker)
/// - For each field:
///   - 4 bytes: field type (IPAD, NAME, JSON, UUID, VERS)
///   - 1 byte: length
///   - N bytes: value
fn parse_tlv_response(data: &[u8], source_addr: &SocketAddr) -> Option<DiscoveredLms> {
    if data.is_empty() {
        return None;
    }

    // First byte should be 'E' (uppercase response) or 'e' (lowercase, echo?)
    let first = data[0];
    if first != b'E' && first != b'e' {
        tracing::debug!("LMS discovery: invalid response marker: {}", first);
        return None;
    }

    let mut fields: HashMap<TlvType, String> = HashMap::new();
    let mut pos = 1; // Skip the 'E' marker

    while pos + 5 <= data.len() {
        // Read 4-byte tag
        let tag = &data[pos..pos + 4];
        pos += 4;

        // Read 1-byte length
        let length = data[pos] as usize;
        pos += 1;

        // Read value
        if pos + length > data.len() {
            tracing::debug!(
                "LMS discovery: truncated value at pos {}, need {}, have {}",
                pos,
                length,
                data.len() - pos
            );
            break;
        }

        let value = &data[pos..pos + length];
        pos += length;

        let field_type = TlvType::from_tag(tag);
        if field_type != TlvType::Unknown {
            // Convert value to string, handling potential UTF-8 issues
            let value_str = String::from_utf8_lossy(value).to_string();
            fields.insert(field_type, value_str);
        }
    }

    // Extract fields, using source address as fallback for IPAD
    let host = fields
        .get(&TlvType::Ipad)
        .cloned()
        .unwrap_or_else(|| source_addr.ip().to_string());

    let name = fields.get(&TlvType::Name).cloned().unwrap_or_default();
    if name.is_empty() {
        tracing::debug!("LMS discovery: no NAME field in response");
        return None;
    }

    // Parse JSON port (defaults to 9000 if not specified or invalid)
    let json_port = fields
        .get(&TlvType::Json)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(9000);

    let uuid = fields.get(&TlvType::Uuid).cloned().unwrap_or_default();
    let version = fields.get(&TlvType::Vers).cloned();

    Some(DiscoveredLms {
        host,
        json_port,
        name,
        uuid,
        version,
    })
}

/// Discover LMS servers on the local network via UDP broadcast
///
/// Sends a broadcast packet to 255.255.255.255:3483 and collects responses.
/// Returns a list of discovered servers, deduplicated by UUID.
pub async fn discover_lms_servers(timeout_ms: Option<u64>) -> Result<Vec<DiscoveredLms>> {
    let timeout_duration = Duration::from_millis(timeout_ms.unwrap_or(LMS_DISCOVERY_TIMEOUT_MS));
    let mut discovered: HashMap<String, DiscoveredLms> = HashMap::new();

    // Bind to any available port
    let socket = UdpSocket::bind("0.0.0.0:0").await?;

    // Enable broadcast
    socket.set_broadcast(true)?;

    // Send discovery request to broadcast address
    let dest: SocketAddr = format!("255.255.255.255:{}", LMS_DISCOVERY_PORT).parse()?;
    socket.send_to(LMS_DISCOVERY_REQUEST, dest).await?;

    tracing::debug!(
        "Sent LMS discovery broadcast to 255.255.255.255:{}",
        LMS_DISCOVERY_PORT
    );

    // Receive responses with timeout
    let mut buf = [0u8; 1024];
    let deadline = tokio::time::Instant::now() + timeout_duration;

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }

        match timeout(remaining, socket.recv_from(&mut buf)).await {
            Ok(Ok((len, addr))) => {
                tracing::debug!("LMS discovery response from {}: {} bytes", addr, len);

                if let Some(server) = parse_tlv_response(&buf[..len], &addr) {
                    // Use UUID for deduplication, fall back to host if no UUID
                    let key = if server.uuid.is_empty() {
                        server.host.clone()
                    } else {
                        server.uuid.clone()
                    };
                    discovered.insert(key, server);
                }
            }
            Ok(Err(e)) => {
                tracing::warn!("LMS discovery recv error: {}", e);
                break;
            }
            Err(_) => {
                // Timeout - done receiving
                break;
            }
        }
    }

    let result: Vec<DiscoveredLms> = discovered.into_values().collect();
    tracing::info!("LMS discovery found {} server(s)", result.len());
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_empty_response() {
        let addr: SocketAddr = "192.168.1.100:3483".parse().unwrap();
        assert!(parse_tlv_response(&[], &addr).is_none());
    }

    #[test]
    fn test_parse_invalid_marker() {
        let addr: SocketAddr = "192.168.1.100:3483".parse().unwrap();
        // Invalid marker 'X' instead of 'E'
        assert!(parse_tlv_response(b"XNAME\x04test", &addr).is_none());
    }

    #[test]
    fn test_parse_minimal_response() {
        let addr: SocketAddr = "192.168.1.100:3483".parse().unwrap();

        // Build a minimal valid response: E + NAME(len=7)"MyMusic"
        let mut response = vec![b'E'];
        response.extend_from_slice(b"NAME");
        response.push(7); // length
        response.extend_from_slice(b"MyMusic");

        let server = parse_tlv_response(&response, &addr).unwrap();
        assert_eq!(server.name, "MyMusic");
        assert_eq!(server.host, "192.168.1.100"); // Falls back to source addr
        assert_eq!(server.json_port, 9000); // Default port
        assert!(server.uuid.is_empty());
        assert!(server.version.is_none());
    }

    #[test]
    fn test_parse_full_response() {
        let addr: SocketAddr = "192.168.1.100:3483".parse().unwrap();

        // Build a full response with all fields
        let mut response = vec![b'E'];

        // IPAD field - "192.168.1.50" is 12 bytes
        let ip = b"192.168.1.50";
        response.extend_from_slice(b"IPAD");
        response.push(ip.len() as u8);
        response.extend_from_slice(ip);

        // NAME field - "Home Music" is 10 bytes
        let name = b"Home Music";
        response.extend_from_slice(b"NAME");
        response.push(name.len() as u8);
        response.extend_from_slice(name);

        // JSON field (port as string) - "9001" is 4 bytes
        let port = b"9001";
        response.extend_from_slice(b"JSON");
        response.push(port.len() as u8);
        response.extend_from_slice(port);

        // UUID field - 36 bytes
        let uuid = b"12345678-1234-1234-1234-123456789abc";
        response.extend_from_slice(b"UUID");
        response.push(uuid.len() as u8);
        response.extend_from_slice(uuid);

        // VERS field - "8.5.1" is 5 bytes
        let vers = b"8.5.1";
        response.extend_from_slice(b"VERS");
        response.push(vers.len() as u8);
        response.extend_from_slice(vers);

        let server = parse_tlv_response(&response, &addr).unwrap();
        assert_eq!(server.host, "192.168.1.50"); // Uses IPAD, not source
        assert_eq!(server.name, "Home Music");
        assert_eq!(server.json_port, 9001);
        assert_eq!(server.uuid, "12345678-1234-1234-1234-123456789abc");
        assert_eq!(server.version, Some("8.5.1".to_string()));
    }

    #[test]
    fn test_parse_response_without_ipad() {
        // When IPAD is not provided, should use the response source address
        let addr: SocketAddr = "10.0.0.5:3483".parse().unwrap();

        let mut response = vec![b'E'];
        response.extend_from_slice(b"NAME");
        response.push(9);
        response.extend_from_slice(b"LMS Server");

        let server = parse_tlv_response(&response, &addr).unwrap();
        assert_eq!(server.host, "10.0.0.5");
    }

    #[test]
    fn test_parse_response_with_invalid_json_port() {
        let addr: SocketAddr = "192.168.1.100:3483".parse().unwrap();

        let mut response = vec![b'E'];
        response.extend_from_slice(b"NAME");
        response.push(4);
        response.extend_from_slice(b"Test");
        response.extend_from_slice(b"JSON");
        response.push(3);
        response.extend_from_slice(b"abc"); // Invalid port

        let server = parse_tlv_response(&response, &addr).unwrap();
        assert_eq!(server.json_port, 9000); // Falls back to default
    }

    #[test]
    fn test_parse_response_missing_name() {
        // Response without NAME field should be rejected
        let addr: SocketAddr = "192.168.1.100:3483".parse().unwrap();

        let mut response = vec![b'E'];
        response.extend_from_slice(b"UUID");
        response.push(4);
        response.extend_from_slice(b"test");

        assert!(parse_tlv_response(&response, &addr).is_none());
    }

    #[test]
    fn test_parse_truncated_response() {
        let addr: SocketAddr = "192.168.1.100:3483".parse().unwrap();

        // Response with truncated value (claims 20 bytes but only has 4)
        let mut response = vec![b'E'];
        response.extend_from_slice(b"NAME");
        response.push(20); // Claims 20 bytes
        response.extend_from_slice(b"test"); // Only 4 bytes

        // Should handle gracefully - no NAME means None
        assert!(parse_tlv_response(&response, &addr).is_none());
    }

    #[test]
    fn test_lowercase_marker() {
        // Some implementations may use lowercase 'e'
        let addr: SocketAddr = "192.168.1.100:3483".parse().unwrap();

        let mut response = vec![b'e']; // lowercase
        response.extend_from_slice(b"NAME");
        response.push(4);
        response.extend_from_slice(b"Test");

        let server = parse_tlv_response(&response, &addr).unwrap();
        assert_eq!(server.name, "Test");
    }

    #[test]
    fn test_discovered_lms_serialization() {
        let server = DiscoveredLms {
            host: "192.168.1.50".to_string(),
            json_port: 9000,
            name: "My Music".to_string(),
            uuid: "test-uuid".to_string(),
            version: Some("8.5.0".to_string()),
        };

        let json = serde_json::to_string(&server).unwrap();
        assert!(json.contains("192.168.1.50"));
        assert!(json.contains("My Music"));
        assert!(json.contains("test-uuid"));
        assert!(json.contains("8.5.0"));

        let deserialized: DiscoveredLms = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, server);
    }

    #[test]
    fn test_discovered_lms_serialization_no_version() {
        // When version is None, it should be omitted from JSON
        let server = DiscoveredLms {
            host: "192.168.1.50".to_string(),
            json_port: 9000,
            name: "My Music".to_string(),
            uuid: "test-uuid".to_string(),
            version: None,
        };

        let json = serde_json::to_string(&server).unwrap();
        assert!(!json.contains("version"));
    }
}
