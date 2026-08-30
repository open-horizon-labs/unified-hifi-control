use std::time::{Duration, SystemTime, UNIX_EPOCH};

use uuid::Uuid;

pub fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis() as u64
}

/// Exact proof-of-possession input used by the private pairing authority.
/// Length-prefixing the human-readable fingerprint avoids ambiguous field
/// concatenation while the UUIDs remain fixed-width binary values.
pub fn possession_message(
    pairing_id: Uuid,
    installation_id: Uuid,
    account_id: Uuid,
    fingerprint: &str,
) -> Vec<u8> {
    let mut message = Vec::with_capacity(128);
    message.extend_from_slice(b"hiphi.installation-pairing.v1\0");
    message.extend_from_slice(pairing_id.as_bytes());
    message.extend_from_slice(installation_id.as_bytes());
    message.extend_from_slice(account_id.as_bytes());
    let fingerprint_bytes = fingerprint.as_bytes();
    let length = u32::try_from(fingerprint_bytes.len()).unwrap_or(u32::MAX);
    message.extend_from_slice(&length.to_be_bytes());
    message.extend_from_slice(fingerprint_bytes);
    message
}
