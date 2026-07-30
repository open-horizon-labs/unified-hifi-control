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
    /// Every query this lane can express, for exhaustive coverage checks.
    pub const ALL: [Query; 11] = [
        Query::GetInfo,
        Query::State,
        Query::Status,
        Query::VolumeRange,
        Query::GetModes,
        Query::GetFilters,
        Query::GetShapers,
        Query::GetRates,
        Query::GetJunkFilters,
        Query::MatrixListProfiles,
        Query::MatrixGetProfile,
    ];

    /// The request element name.
    pub fn element(self) -> &'static str {
        match self {
            Query::GetInfo => "GetInfo",
            Query::State => "State",
            Query::Status => "Status",
            Query::VolumeRange => "VolumeRange",
            Query::GetModes => "GetModes",
            Query::GetFilters => "GetFilters",
            Query::GetShapers => "GetShapers",
            Query::GetRates => "GetRates",
            Query::GetJunkFilters => "GetJunkFilters",
            Query::MatrixListProfiles => "MatrixListProfiles",
            Query::MatrixGetProfile => "MatrixGetProfile",
        }
    }

    /// The element the daemon answers with.
    ///
    /// Stated per query rather than assumed equal to the request. Every family the reference documents
    /// does echo its request element, and this mapping records that as a checked fact — if a future
    /// daemon answers a query under a different name, the fix is one line here instead of a timeout
    /// nobody can explain.
    /// Every family the reference documents answers under its own request element, so today this is the
    /// identity. It exists as a separate function anyway: the moment one family answers under a
    /// different name, the fix is one arm here rather than a timeout nobody can explain. The matrix
    /// pair was checked specifically, being the likeliest to diverge — one is a container of
    /// `MatrixProfile` children, the other a single current-selection element — and both echo.
    pub fn reply_element(self) -> &'static str {
        self.element()
    }

    /// The capture key this query's evidence is filed under, so the mapping lives with the query
    /// instead of in a parallel list that can drift out of step with `ALL`.
    pub fn capture_family(self) -> &'static str {
        match self {
            Query::GetInfo => "getinfo",
            Query::State => "state",
            Query::Status => "status",
            Query::VolumeRange => "volume_range",
            Query::GetModes => "modes",
            Query::GetFilters => "filters",
            Query::GetShapers => "shapers",
            Query::GetRates => "rates",
            Query::GetJunkFilters => "junkfilters",
            Query::MatrixListProfiles => "matrix",
            Query::MatrixGetProfile => "matrix_current",
        }
    }

    /// The full request document. No caller-supplied text reaches it, so there is no interpolation
    /// point at which a command could be appended.
    pub fn request(self) -> String {
        let attrs = match self {
            // One-shot only. Push mode is not something this lane may enable.
            Query::Status => " subscribe=\"0\"",
            _ => "",
        };
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><{}{attrs}/>\n",
            self.element()
        )
    }
}

/// Read one raw reply document for a query element.
///
/// `element` must name a query; anything else is refused rather than sent, so the read-only guarantee
/// is enforced here and not left to the caller's care.
pub async fn observe(
    host: &str,
    port: u16,
    query: Query,
    response_deadline: Duration,
) -> anyhow::Result<String> {
    let element = query.element();
    let expected_reply = query.reply_element();

    let stream = tokio::time::timeout(response_deadline, TcpStream::connect((host, port)))
        .await
        .map_err(|_| anyhow::anyhow!("raw lane connect timeout"))??;
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    let request = query.request();
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
                        if framing::root_element(&document).as_deref() == Some(expected_reply) {
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
    let mut reader = quick_xml::Reader::from_str(document);
    reader.config_mut().check_end_names = false;
    loop {
        match reader.read_event() {
            Ok(quick_xml::events::Event::Start(e)) | Ok(quick_xml::events::Event::Empty(e)) => {
                if String::from_utf8_lossy(e.name().as_ref()) == child {
                    return true;
                }
            }
            Ok(quick_xml::events::Event::Eof) | Err(_) => return false,
            Ok(_) => {}
        }
    }
}

/// Every `(name, attribute-map)` pair for a repeated child element, so attribute *presence* can be
/// distinguished from attribute *value*.
pub fn child_attrs(document: &str, child: &str) -> Vec<Vec<(String, String)>> {
    let mut out = Vec::new();
    let mut reader = quick_xml::Reader::from_str(document);
    reader.config_mut().check_end_names = false;
    loop {
        match reader.read_event() {
            Ok(quick_xml::events::Event::Start(e)) | Ok(quick_xml::events::Event::Empty(e)) => {
                if String::from_utf8_lossy(e.name().as_ref()) == child {
                    out.push(
                        e.attributes()
                            .filter_map(Result::ok)
                            .map(|a| {
                                let name = String::from_utf8_lossy(a.key.as_ref()).into_owned();
                                let value = a
                                    .unescape_value()
                                    .map(|v| v.into_owned())
                                    .unwrap_or_else(|_| {
                                        framing::decode_entities(&String::from_utf8_lossy(&a.value))
                                    });
                                (name, value)
                            })
                            .collect(),
                    );
                }
            }
            Ok(quick_xml::events::Event::Eof) | Err(_) => return out,
            Ok(_) => {}
        }
    }
}

/// Attributes of a document's root element.
pub fn root_attrs(document: &str) -> Vec<(String, String)> {
    element_attrs(document)
}

/// Attributes of the first element in a fragment, parsed with quick-xml.
///
/// Hand-rolled scanning is what made the previous version silently under-observe: it assumed double
/// quotes and no whitespace around `=`, so `index = '0'` yielded nothing at all and the diff reported
/// a false absence. quick-xml is already in the dependency graph and handles both quote styles,
/// whitespace, and entity decoding, so the parser cannot be more restrictive than the daemon.
fn element_attrs(fragment: &str) -> Vec<(String, String)> {
    let mut reader = quick_xml::Reader::from_str(fragment);
    reader.config_mut().check_end_names = false;
    loop {
        match reader.read_event() {
            Ok(quick_xml::events::Event::Start(e)) | Ok(quick_xml::events::Event::Empty(e)) => {
                return e
                    .attributes()
                    .filter_map(Result::ok)
                    .map(|a| {
                        let name = String::from_utf8_lossy(a.key.as_ref()).into_owned();
                        let value =
                            a.unescape_value()
                                .map(|v| v.into_owned())
                                .unwrap_or_else(|_| {
                                    framing::decode_entities(&String::from_utf8_lossy(&a.value))
                                });
                        (name, value)
                    })
                    .collect();
            }
            Ok(quick_xml::events::Event::Eof) | Err(_) => return Vec::new(),
            Ok(_) => {}
        }
    }
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
