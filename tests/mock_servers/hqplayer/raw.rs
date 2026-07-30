//! Test-only, strictly read-only raw observation lane.
//!
//! Some claims ADR 003 requires cannot be observed through the adapter's semantic types at all:
//! `FilterItem` has no `description` field, and a `Status` document's `metadata` child either exists
//! or does not regardless of the values inside it. Inferring those from parsed values is what produced
//! two defects in this harness already, so they are **observed** instead.
//!
//! This lane opens its own connection, sends only query elements, and frames replies with the
//! **production** `framing` code, so what it observes is what the client would see. It sends no
//! `Set*`, no `Volume*`, no transport and no matrix-set command — there is no code path here that
//! could change a daemon's state.
//!
//! Nothing it returns carries the address it connected to. Callers hand it a host, it hands back
//! documents.

use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

use unified_hifi_control::adapters::hqplayer::framing;

/// Query elements this lane is permitted to send. Read-only by enumeration, not by intention: adding
/// a mutating element here would be an obvious review flag rather than a silent behaviour change.
pub const READ_ONLY_QUERIES: [&str; 8] = [
    "GetInfo",
    "State",
    "Status",
    "VolumeRange",
    "GetModes",
    "GetFilters",
    "GetShapers",
    "GetRates",
];

/// A closed set of read-only requests this lane can express.
///
/// Deliberately not a free-form attribute string: with `attrs: &str` interpolated into the request, a
/// caller could close the element and append anything - `/><Volume value="0"` - and the read-only
/// guarantee would rest on caller discipline rather than on the type. Every variant here renders to a
/// query and there is no variant that can render a mutating command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Query {
    GetInfo,
    /// One-shot status. `subscribe` is fixed to 0: push mode is not something this lane may enable.
    Status,
    State,
    VolumeRange,
    GetModes,
    GetFilters,
    GetShapers,
    GetRates,
    GetJunkFilters,
    MatrixListProfiles,
    MatrixGetProfile,
}

impl Query {
    /// The request element name.
    pub fn element(self) -> &'static str {
        "STUB"
    }

    /// The element name the daemon answers with. Usually the same, but not guaranteed for every
    /// family, and assuming it silently turns a naming difference into a timeout.
    pub fn reply_element(self) -> &'static str {
        "STUB"
    }

    /// The full request document. No caller-supplied text reaches it.
    pub fn request(self) -> String {
        String::new()
    }
}

/// Read one raw reply document for a query element.
///
/// `element` must name a query; anything else is refused rather than sent, so the read-only guarantee
/// is enforced here and not left to the caller's care.
pub async fn observe(
    host: &str,
    port: u16,
    element: &str,
    attrs: &str,
    response_deadline: Duration,
) -> anyhow::Result<String> {
    anyhow::ensure!(
        READ_ONLY_QUERIES.contains(&element),
        "the raw lane refuses {element}: it sends queries only"
    );

    let stream = tokio::time::timeout(response_deadline, TcpStream::connect((host, port)))
        .await
        .map_err(|_| anyhow::anyhow!("raw lane connect timeout"))??;
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    let request = format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?><{element}{attrs}/>\n");
    write_half.write_all(request.as_bytes()).await?;
    write_half.flush().await?;

    // Same rule the client follows: read until a document parses. Bounded by one overall deadline so a
    // chatty daemon cannot hold this open.
    let deadline = tokio::time::Instant::now() + response_deadline;
    let mut document = String::new();
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        anyhow::ensure!(!remaining.is_zero(), "raw lane response timeout");
        let mut line = String::new();
        match tokio::time::timeout(remaining, reader.read_line(&mut line)).await {
            Ok(Ok(0)) => anyhow::bail!("raw lane: connection closed mid-document"),
            Ok(Ok(_)) => {
                document.push_str(&line);
                match framing::classify(&document) {
                    framing::Framing::Complete => {
                        if framing::root_element(&document).as_deref() == Some(element) {
                            return Ok(document);
                        }
                        // Unsolicited push frame; drop it and keep reading, exactly as the client does.
                        document.clear();
                    }
                    framing::Framing::Malformed => {
                        anyhow::bail!("raw lane: malformed document for {element}")
                    }
                    framing::Framing::Incomplete => {}
                }
            }
            Ok(Err(e)) => anyhow::bail!("raw lane read error: {e}"),
            Err(_) => anyhow::bail!("raw lane response timeout"),
        }
    }
}

/// Whether a container's named child element is present, as a structural fact rather than an
/// inference from any value inside it.
pub fn has_child(document: &str, child: &str) -> bool {
    document.contains(&format!("<{child}"))
}

/// Every `(name, attribute-map)` pair for a repeated child element, so attribute *presence* can be
/// distinguished from attribute *value*.
pub fn child_attrs(document: &str, child: &str) -> Vec<Vec<(String, String)>> {
    document
        .split(&format!("<{child}"))
        .skip(1)
        .filter_map(|part| {
            let end = part.find("/>")?;
            Some(parse_attrs(&part[..end]))
        })
        .collect()
}

/// Attributes of a document's root element.
pub fn root_attrs(document: &str) -> Vec<(String, String)> {
    framing::root_open_tag(document)
        .map(parse_attrs)
        .unwrap_or_default()
}

fn parse_attrs(fragment: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let bytes = fragment.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // name=
        while i < bytes.len() && (bytes[i] as char).is_whitespace() {
            i += 1;
        }
        let start = i;
        while i < bytes.len() && bytes[i] != b'=' && !(bytes[i] as char).is_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'=' {
            break;
        }
        let name = fragment[start..i].trim().to_string();
        i += 1;
        if i >= bytes.len() || bytes[i] != b'"' {
            break;
        }
        i += 1;
        let vstart = i;
        while i < bytes.len() && bytes[i] != b'"' {
            i += 1;
        }
        let value = fragment[vstart..i.min(fragment.len())].to_string();
        i += 1;
        if !name.is_empty() {
            out.push((name, framing::decode_entities(&value)));
        }
    }
    out
}

/// Strip anything that must never reach a stored artifact: hidden form inputs, anything that looks
/// like a token or credential, and `Authorization`-style headers.
///
/// Applied to every raw document before it is recorded, so the sanitiser is not something a caller can
/// forget to invoke.
pub fn sanitize(document: &str) -> String {
    let mut out = String::with_capacity(document.len());
    let mut rest = document;
    while let Some(at) = rest.find('<') {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        let end = tail.find('>').map(|e| e + 1).unwrap_or(tail.len());
        let tag = &tail[..end];
        let lower = tag.to_lowercase();
        let sensitive = [
            "hidden",
            "password",
            "passwd",
            "csrf",
            "token",
            "nonce",
            "authorization",
        ]
        .iter()
        .any(|needle| lower.contains(needle));
        if sensitive {
            out.push_str("<!-- redacted: sensitive attribute or hidden input -->");
        } else {
            out.push_str(tag);
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}
