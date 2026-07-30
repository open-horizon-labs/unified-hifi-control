//! HQPlayer Native Protocol Client + HTTP/Web Client for Profiles
//!
//! Implements the TCP/XML control protocol on port 4321 for pipeline control.
//! Also implements HTTP/Digest auth for web UI profile loading (port 8088).
//! Based on Jussi Laako's hqp-control reference implementation.
//!
//! **Protocol authority is the executable corpus**, not this file and not prose: see
//! `tests/fixtures/hqplayer/<version>/` driven by `tests/hqplayer_conformance.rs`, and
//! `docs/adr/003-hqplayer-conformance-boundary.md` for why. `docs/hqplayer-protocol-reference.md`
//! is a reader's guide with a table of the claims the corpus overturned.
//!
//! This comment previously pointed at `docs/hqplayer-protocol-audit.md`, which no longer exists —
//! and the doc that replaced it was silent on the `result` attribute, which is how this client came
//! to report success for commands the daemon had rejected. Hence the corpus.
//!
//! Semantics worth knowing before reading on:
//! - `State` reports settings as **list indices**; `Status` reports the *active* filter/shaper/mode
//!   as display strings. The two are different domains and must not be mixed.
//! - `hqplayerd.xml` stores enum **IDs**, which are a third domain again.
//! - Enumerations are mode-relative: a mode change swaps the filter/shaper/rate lists wholesale.
//! - `result="OK"` is not proof a setting applied. See `verify_applied`.

use anyhow::{anyhow, Result};
use quick_xml::events::{BytesStart, Event};
use quick_xml::Writer;
use regex::Regex;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Cursor;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::{Mutex, RwLock};
use tokio::time::timeout;

use crate::bus::{
    BusEvent, NowPlaying as BusNowPlaying, PlaybackState, PrefixedZoneId, SharedBus, TrackMetadata,
    VolumeControl as BusVolumeControl, VolumeScale, Zone as BusZone,
};
use crate::config::{get_config_file_path, read_config_file};

const HQP_CONFIG_FILE: &str = "hqp-config.json";

/// Saved config for persistence (single instance format)
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SavedHqpConfig {
    host: String,
    port: u16,
    web_port: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    password: Option<String>,
}

/// Named instance config (for multi-instance support)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HqpInstanceConfig {
    pub name: String,
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_web_port")]
    pub web_port: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

fn default_port() -> u16 {
    DEFAULT_PORT
}

fn default_web_port() -> u16 {
    DEFAULT_WEB_PORT
}

fn hqp_config_path() -> PathBuf {
    get_config_file_path(HQP_CONFIG_FILE)
}

/// Load HQP config from disk (supports both single-object and array formats)
/// Issue #76: Uses read_config_file for backwards-compatible fallback
pub fn load_hqp_configs() -> Vec<HqpInstanceConfig> {
    // read_config_file checks subdir first, falls back to root for legacy files
    let content = match read_config_file(HQP_CONFIG_FILE) {
        Some(c) => c,
        None => return Vec::new(),
    };

    // Try parsing as array first
    if let Ok(configs) = serde_json::from_str::<Vec<HqpInstanceConfig>>(&content) {
        return configs;
    }

    // Fall back to single-object format (legacy)
    if let Ok(single) = serde_json::from_str::<SavedHqpConfig>(&content) {
        return vec![HqpInstanceConfig {
            name: "default".to_string(),
            host: single.host,
            port: single.port,
            web_port: single.web_port,
            username: single.username,
            password: single.password,
        }];
    }

    tracing::warn!("Failed to parse HQP config file");
    Vec::new()
}

/// Save HQP configs to disk (always saves as array)
pub fn save_hqp_configs(configs: &[HqpInstanceConfig]) -> bool {
    let path = hqp_config_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    match serde_json::to_string_pretty(configs) {
        Ok(json) => match std::fs::write(&path, json) {
            Ok(()) => {
                tracing::info!("Saved HQP config ({} instances)", configs.len());
                true
            }
            Err(e) => {
                tracing::error!("Failed to save HQP config: {}", e);
                false
            }
        },
        Err(e) => {
            tracing::error!("Failed to serialize HQP config: {}", e);
            false
        }
    }
}

/// Response framing for the native control protocol.
///
/// The daemon sends newline-*terminated* XML documents, not newline-*framed* ones: a document may
/// contain internal newlines, and container responses normally do. The reference client therefore
/// reads until a complete document parses, treating a premature end-of-document as "keep reading".
///
/// A reader that stops at the first `/>` misframes any container with a self-closing child — most
/// importantly `<Status …><metadata …/></Status>` — and leaves the closing tag in the socket for
/// the next command to consume as its own reply.
///
/// Reference: <https://github.com/ohshitgorillas/hqptuner/blob/67557939ae04b157b47cb67bd651b72c3140bcdd/docs/protocol.md>
/// §1 (transport and framing) and §6 (`Status`).
pub mod framing {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    /// Whether the bytes read so far form a complete protocol document.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Framing {
        /// A well-formed document has not ended yet — keep reading.
        Incomplete,
        /// Exactly one top-level element has opened and closed.
        Complete,
        /// The bytes cannot become a valid document however much more arrives.
        Malformed,
    }

    /// Classify accumulated response bytes.
    ///
    /// Pure and cheap, so it can be exercised exhaustively at every byte offset of a document
    /// without a socket.
    pub fn classify(buf: &str) -> Framing {
        match scan(buf) {
            Scan::Complete(_) => Framing::Complete,
            Scan::Incomplete => Framing::Incomplete,
            Scan::Malformed => Framing::Malformed,
        }
    }

    /// Byte offset just past the end of the **first** complete document in `buf`.
    ///
    /// `None` when the buffer holds no complete document, for either reason. This is what lets a
    /// caller drop one document from an accumulated buffer while keeping whatever was coalesced
    /// behind it: discarding the whole buffer would throw away a reply that had already arrived in the
    /// same read.
    pub fn first_document_end(buf: &str) -> Option<usize> {
        match scan(buf) {
            Scan::Complete(end) => Some(end),
            _ => None,
        }
    }

    /// Outcome of one framing walk. [`classify`] and [`first_document_end`] are both projections of
    /// it, so there is one traversal to keep correct rather than two that can disagree.
    enum Scan {
        /// A document ended at this byte offset.
        Complete(usize),
        Incomplete,
        Malformed,
    }

    fn scan(buf: &str) -> Scan {
        // A buffer whose first element is a closing tag can never become a document, however much
        // more arrives. This is exactly the leftover a truncating reader produces, so catching it
        // here turns a silent desync into a reported error. quick_xml raises a generic parse error
        // for it, which is indistinguishable from "truncated mid-token", hence the explicit check.
        if let Some(rest) = skip_prologue(buf) {
            if rest.starts_with("</") {
                return Scan::Malformed;
            }
        }

        // `check_end_names` stays off so quick_xml does not collapse a name mismatch into a generic
        // parse error, which would be indistinguishable from "truncated mid-token". The stack of
        // open element names below does that checking instead, so a mismatch reads as malformed
        // while a truncation still reads as incomplete.
        let mut reader = Reader::from_str(buf);
        reader.config_mut().check_end_names = false;
        let mut open: Vec<String> = Vec::new();

        loop {
            match reader.read_event() {
                Ok(Event::Start(e)) => {
                    open.push(String::from_utf8_lossy(e.name().as_ref()).into_owned());
                }
                Ok(Event::End(e)) => {
                    let name = e.name();
                    let closing = String::from_utf8_lossy(name.as_ref());
                    match open.pop() {
                        // Depth counting alone accepts `<State …></Status>`: one element opened and
                        // one closed. Comparing names is what makes that a rejection.
                        Some(expected) if expected.as_str() != closing.as_ref() => {
                            return Scan::Malformed
                        }
                        // A closing tag with nothing open: the leftover tail of a document that a
                        // previous read truncated. Never a valid document on its own.
                        None => return Scan::Malformed,
                        Some(_) if open.is_empty() => {
                            return Scan::Complete(reader.buffer_position() as usize)
                        }
                        Some(_) => {}
                    }
                }
                Ok(Event::Empty(_)) => {
                    if open.is_empty() {
                        return Scan::Complete(reader.buffer_position() as usize);
                    }
                }
                // Ran out of bytes, or hit a token quick_xml cannot read. Normally that means more is
                // coming — but if the root's own closing tag is already present, the frame *is* closed
                // and only the children are unreadable. See `root_frame_end`.
                Ok(Event::Eof) | Err(_) => {
                    return match root_frame_end(buf) {
                        Some(end) => Scan::Complete(end),
                        None => Scan::Incomplete,
                    }
                }
                // Declaration, text, comments and processing instructions carry no framing weight.
                Ok(_) => {}
            }
        }
    }

    /// Whether the buffer ends with the root element's own closing tag.
    ///
    /// This is the recovery boundary for a document whose *children* cannot be parsed. The daemon
    /// emits malformed XML inside `<metadata>`, and a child tag that never terminates makes a parser
    /// consume the `</Status>` that follows it as part of the child's own attribute soup — so the
    /// closing tag is in the buffer but was never seen as an end event. Without recovery such a
    /// reply reads as incomplete and the command burns its whole deadline waiting for bytes it
    /// already holds, on **every** poll while a track is loaded.
    ///
    /// Deliberately narrow. Each clause protects a framing guarantee #322 exists to hold:
    ///
    /// * It keys on the root's **own** name, so `<State …></Status>` is not rescued — mismatched
    ///   nesting is decided by the name-comparison path above, which is reached first.
    /// * It requires the closing tag to be the buffer's **last** token, so a document truncated
    ///   mid-child stays incomplete instead of being credited with a tag that has not arrived, and
    ///   a `</Status>` appearing inside an attribute value cannot pass for a frame end.
    /// * It is consulted only when the parse could not complete on its own, so every well-formed
    ///   document takes the unchanged path.
    ///
    /// Reaching here with the root's closing tag present is itself the diagnosis: had that tag been
    /// parsed as an end event, the element stack would have emptied and `Complete` would already
    /// have been returned.
    ///
    /// Attribute reads stay correct either way — [`root_open_tag`] is a quote-aware scan that stops
    /// at the root tag's own `>`, so it never looks at a child.
    ///
    /// The root's name comes from [`root_element`] rather than a private scan, so this shares one
    /// tokeniser with the rest of the module. A self-closing root cannot reach here at all: quick_xml
    /// reports it as `Event::Empty`, which the loop above answers before any child is read.
    ///
    /// The boundary is the **first defensible** occurrence of the root's closing tag, not the last
    /// token in the buffer. Requiring it last was simpler but wrong in a case the daemon actually
    /// produces: it emits malformed XML inside `<metadata>` *and* pushes `Status` frames unprompted,
    /// so a hostile reply with a push frame coalesced behind it would have gone unrecovered and cost
    /// the whole deadline. "First defensible" keeps the guarantees that mattered:
    ///
    /// * The scan tracks attribute quoting, so a `</Status>` literal inside an attribute value is
    ///   data and never a boundary.
    /// * A document truncated before its root close has no boundary to find, so it stays incomplete.
    /// * A mismatched root is decided by the name-comparison path above, which is reached first.
    ///
    /// An *unterminated* attribute quote is the one shape this cannot resolve, and deliberately so:
    /// with the quote still open there is no way to tell markup from data, so the scan declines to
    /// find a boundary rather than guessing at one.
    fn root_frame_end(buf: &str) -> Option<usize> {
        let name = root_element(buf)?;
        let close = format!("</{name}>");
        let bytes = buf.as_bytes();
        // Which quote character opened the attribute value currently being scanned, if any. XML
        // permits both forms, and only the *matching* character closes a value — a `"` inside a
        // single-quoted value is content. Tracking a single bool for `"` alone would let
        // `song='</Status>'` end the frame, so a truncated document would read as complete.
        let mut quote: Option<u8> = None;
        let mut i = 0;
        while i < bytes.len() {
            // Regions where a closing-tag literal is content, not markup. Skipping them wholesale is
            // what stops `<!-- </Status> -->` being read as a frame boundary — which would let a
            // *truncated* document pass as complete, the one failure this whole function exists to
            // avoid. Quoting alone does not cover these: none of them is quoted.
            if quote.is_none() {
                if let Some(skip) = non_markup_region_len(&buf[i..]) {
                    i += skip;
                    continue;
                }
            }
            match bytes[i] {
                b'"' | b'\'' => match quote {
                    // Only the character that opened the value can close it.
                    Some(open) if open == bytes[i] => quote = None,
                    Some(_) => {}
                    None => quote = Some(bytes[i]),
                },
                b'<' if quote.is_none() && buf[i..].starts_with(&close) => {
                    return Some(i + close.len())
                }
                _ => {}
            }
            i += 1;
        }
        None
    }

    /// Length of a region starting at `rest` in which `<` is **not** markup, if one starts there.
    ///
    /// This list is the whole point of the function, and it is **closed rather than open-ended**: XML
    /// defines exactly four places a `<` can appear without opening a tag — a quoted attribute value
    /// (handled by the caller's quote tracking), a comment, a CDATA section, and a processing
    /// instruction. Declarations beginning `<!` that are neither comment nor CDATA — in practice a
    /// `DOCTYPE`, which is only legal in the prologue and so already malformed here — are skipped to
    /// their closing `>` conservatively, for the same reason.
    ///
    /// It was worth enumerating rather than extending one case at a time. Three of these were found
    /// one at a time by probing, each after the previous was called complete; the fourth was found by
    /// asking what the *set* is instead of what the next example might be. Note that
    /// [`skip_prologue`] already knew this vocabulary for the prologue — the same knowledge simply had
    /// not been applied mid-document.
    ///
    /// An unterminated region consumes the remainder: there is no boundary inside something that has
    /// not ended, and treating its contents as markup is exactly the mistake this prevents.
    fn non_markup_region_len(rest: &str) -> Option<usize> {
        for (open, close) in [
            ("<!--", "-->"),
            ("<![CDATA[", "]]>"),
            // Processing instruction. Must be tried before the bare `<!` fallback below, and after
            // the two longer `<!` forms above, so the most specific prefix wins.
            ("<?", "?>"),
        ] {
            if let Some(body) = rest.strip_prefix(open) {
                return Some(match body.find(close) {
                    Some(at) => open.len() + at + close.len(),
                    None => rest.len(),
                });
            }
        }
        // Any other declaration: skip to its closing `>`.
        if rest.starts_with("<!") {
            return Some(match rest.find('>') {
                Some(at) => at + 1,
                None => rest.len(),
            });
        }
        None
    }

    /// Skip leading whitespace, XML declarations, comments and processing instructions.
    /// Returns `None` when nothing but prologue has arrived yet.
    fn skip_prologue(buf: &str) -> Option<&str> {
        let mut rest = buf.trim_start();
        loop {
            if rest.is_empty() {
                return None;
            }
            let closing = if rest.starts_with("<?") {
                "?>"
            } else if rest.starts_with("<!--") {
                "-->"
            } else if rest.starts_with("<!") {
                ">"
            } else {
                return Some(rest);
            };
            let end = rest.find(closing)? + closing.len();
            rest = rest[end..].trim_start();
        }
    }

    /// The root element's opening tag, including its attributes.
    ///
    /// Attribute lookups must be scoped to this slice: a plain substring scan over the whole
    /// document matches the XML declaration's `version="1.0"` before the root element's own
    /// `version`, silently replacing the daemon's version with the XML spec's.
    pub fn root_open_tag(buf: &str) -> Option<&str> {
        let rest = skip_prologue(buf)?;
        if !rest.starts_with('<') || rest.starts_with("</") {
            return None;
        }
        let bytes = rest.as_bytes();
        let mut in_quote = false;
        for (i, b) in bytes.iter().enumerate() {
            match b {
                b'"' => in_quote = !in_quote,
                b'>' if !in_quote => return Some(&rest[..=i]),
                _ => {}
            }
        }
        None
    }

    /// Text content of a document's root element, e.g. the reason in
    /// `<SetFilter result="Error">invalid filter</SetFilter>`.
    pub fn root_text(buf: &str) -> Option<String> {
        let open = root_open_tag(buf)?;
        let rest = skip_prologue(buf)?;
        let after = &rest[open.len()..];
        let end = after.find("</")?;
        let text = after[..end].trim();
        if text.is_empty() {
            None
        } else {
            Some(decode_entities(text))
        }
    }

    /// Decode the XML entities the daemon uses in attribute values and element text.
    ///
    /// The reference warns that string attributes can arrive entity-escaped, and that a bare `&`
    /// has also been observed, so be lenient in both directions: an unrecognised `&…` sequence is
    /// left exactly as it was rather than being dropped.
    pub fn decode_entities(raw: &str) -> String {
        if !raw.contains('&') {
            return raw.to_string();
        }
        let mut out = String::with_capacity(raw.len());
        let mut rest = raw;
        while let Some(at) = rest.find('&') {
            out.push_str(&rest[..at]);
            let tail = &rest[at..];
            // Only look for the terminating `;` within the longest a real reference can be: the
            // named ones are at most `&apos;`/`&quot;` (6 chars) and the longest numeric form is
            // `&#x10FFFF;` (10). Beyond that the `;` belongs to later text, not to this `&`, so
            // treating it as an entity would swallow the words in between.
            let Some(semi) = tail.find(';').filter(|s| *s <= 10) else {
                // Bare ampersand: keep it and move on.
                out.push('&');
                rest = &tail[1..];
                continue;
            };
            let entity = &tail[1..semi];
            let decoded = match entity {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                _ => entity
                    .strip_prefix('#')
                    .and_then(
                        |n| match n.strip_prefix('x').or_else(|| n.strip_prefix('X')) {
                            Some(hex) => u32::from_str_radix(hex, 16).ok(),
                            None => n.parse::<u32>().ok(),
                        },
                    )
                    .and_then(char::from_u32),
            };
            match decoded {
                Some(c) => {
                    out.push(c);
                    rest = &tail[semi + 1..];
                }
                None => {
                    out.push('&');
                    rest = &tail[1..];
                }
            }
        }
        out.push_str(rest);
        out
    }

    /// Name of the single top-level element of a complete document.
    pub fn root_element(buf: &str) -> Option<String> {
        let mut reader = Reader::from_str(buf);
        reader.config_mut().check_end_names = false;
        loop {
            match reader.read_event() {
                Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                    return Some(String::from_utf8_lossy(e.name().as_ref()).into_owned())
                }
                Ok(Event::Eof) | Err(_) => return None,
                Ok(_) => {}
            }
        }
    }
}

/// Timeout and retry policy for the native control connection.
///
/// Injectable so the conformance suite can exercise timeout and reconnect boundaries without
/// waiting on a wall clock. Defaults reproduce the shipped constants exactly, so production
/// behaviour is unchanged unless a caller opts in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HqpTimeouts {
    pub connect: Duration,
    /// Overall ceiling on one command: how long `send_command_inner` may spend between writing the
    /// request and holding its complete reply. Deliberately a *whole-command* budget rather than a
    /// per-read one, so a daemon streaming unsolicited documents cannot keep a reply-less command
    /// alive indefinitely by resetting the clock on every frame.
    pub response: Duration,
    /// Backoff between reconnect attempts. **Also** paces `verify_applied`'s readback polling: one
    /// value covers both network retry backoff and daemon apply-latency backoff. That reuse avoids a
    /// third knob, but the coupling is real — if #328/#329 need to tune apply latency independently
    /// of reconnect backoff, split them then rather than discovering it the hard way.
    pub reconnect_delay: Duration,
    pub max_attempts: u32,
}

impl Default for HqpTimeouts {
    fn default() -> Self {
        Self {
            connect: CONNECT_TIMEOUT,
            response: RESPONSE_TIMEOUT,
            reconnect_delay: RECONNECT_DELAY,
            max_attempts: MAX_RECONNECT_ATTEMPTS,
        }
    }
}

const DEFAULT_PORT: u16 = 4321;
const DEFAULT_WEB_PORT: u16 = 8088;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(3);
const PROFILE_PATH: &str = "/config/profile/load";
/// Maximum reconnection attempts before giving up
const MAX_RECONNECT_ATTEMPTS: u32 = 2;
/// Delay between reconnection attempts (HQPlayer can be overwhelmed by rapid connections)
const RECONNECT_DELAY: Duration = Duration::from_secs(1);
/// Backlog ceiling on unsolicited documents skipped within one command. **Not** the time bound.
///
/// The time bound is the per-command deadline in `send_command_inner`: skipping a document consumes
/// the same budget as waiting for one, so unsolicited traffic cannot extend how long a command waits.
/// This constant only stops unbounded work if frames arrive faster than the deadline is noticed, so
/// it is deliberately private and generous — it is CPU protection, not policy, and nothing should
/// need to tune it.
///
/// An earlier revision made this a public, derived `max_unsolicited = 64` instead of fixing the
/// deadline. That was wrong: `response` bounds a single *read*, so every skip reset it and a steady
/// 1-2 Hz push stream kept a reply-less command alive for tens of seconds — worse than the 8 it
/// replaced.
const MAX_UNSOLICITED_BACKLOG: u32 = 256;

/// Byte ceiling on one command's accumulated reply. The **memory** bound, and the third of three.
///
/// The other two are the per-command deadline (time) and [`MAX_UNSOLICITED_BACKLOG`] (frame count).
/// Neither bounds memory: a container that never closes grows this buffer for as long as the
/// deadline allows, at whatever rate the link sustains, and a fast link can deliver a great deal in
/// a few seconds. Distinguishing "oversized" from "slow" also matters diagnostically — a timeout
/// sends an operator looking at the network, and this sends them looking at the reply.
///
/// 4 MiB against a largest-observed container of a 77-entry filter list, a few KB: roughly three
/// orders of magnitude of headroom, so no legitimate document can approach it. Deliberately private
/// for the same reason as its sibling — it is protection, not policy, and nothing should tune it.
/// It matches the ceiling the reference implementation independently chose.
///
/// Checked after appending a read, so the true peak is this plus one line. That is deliberate — a
/// mid-line check would need the reader to hand back a partial line — and it is stated because
/// "4 MiB" would otherwise read as a hard bound on the allocation rather than on the accumulation.
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

/// Size of one socket read while accumulating a reply.
///
/// Fixed and stack-allocated, which is what makes [`MAX_RESPONSE_BYTES`] a bound on *allocation*: the
/// ceiling is compared against `already_held + n` before anything is appended, so the accumulated
/// buffer can never exceed it and the read itself can never exceed this. 8 KiB comfortably holds the
/// largest observed container in one or two reads without making a small reply pay for a large
/// buffer.
const RESPONSE_READ_CHUNK: usize = 8 * 1024;

/// HQPlayer state information
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HqpState {
    pub state: u8, // 0=stopped, 1=paused, 2=playing
    pub mode: u8,  // PCM=0, SDM=1
    pub filter: u32,
    pub filter1x: Option<u32>,
    pub filter_nx: Option<u32>,
    pub shaper: u32,
    pub rate: u32,
    /// Rounded dB, kept for payload compatibility. Prefer `volume_db`.
    pub volume: i32,
    /// Exact dB as the daemon sent it. Skipped by serde so no response payload changes.
    #[serde(skip)]
    pub volume_db: f64,
    /// `filter_junk` list index. The wire attribute is an int index into `GetJunkFilters`, not the
    /// boolean `filter_20k` below, which is retained only for payload compatibility.
    #[serde(skip)]
    pub filter_junk: u32,
    pub active_mode: u8,
    pub active_rate: u32,
    pub invert: bool,
    pub convolution: bool,
    pub repeat: u8, // 0=off, 1=track, 2=all
    pub random: bool,
    pub adaptive: bool,
    pub filter_20k: bool,
    pub matrix_profile: String,
}

/// HQPlayer info
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HqpInfo {
    pub name: String,
    pub product: String,
    pub version: String,
    pub platform: String,
    pub engine: String,
}

/// HQPlayer playback status
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HqpStatus {
    pub state: u8,
    pub track: u32,
    pub track_id: String,
    pub position: u32,
    pub length: u32,
    /// Rounded dB, kept for payload compatibility. Prefer `volume_db`.
    pub volume: i32,
    /// Exact dB as the daemon sent it. Skipped by serde so no response payload changes.
    #[serde(skip)]
    pub volume_db: f64,
    pub active_mode: String,
    pub active_filter: String,
    pub active_shaper: String,
    pub active_rate: u32,
    pub active_bits: u32,
    pub active_channels: u32,
    pub samplerate: u32,
    pub bitrate: u32,
}

/// Volume range info
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VolumeRange {
    /// Rounded dB, kept for payload compatibility. Prefer the `_db` fields.
    pub min: i32,
    pub max: i32,
    pub step: i32,
    pub enabled: bool,
    pub adaptive: bool,
    /// Exact dB bounds as the daemon sent them. Skipped by serde so no payload changes.
    #[serde(skip)]
    pub min_db: f64,
    #[serde(skip)]
    pub max_db: f64,
    /// `None` when the daemon sends no `step` attribute, which the verified sample does not.
    #[serde(skip)]
    pub step_db: Option<f64>,
}

/// Mode/Filter/Shaper item
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListItem {
    pub index: u32,
    pub name: String,
    pub value: i32, // Can be negative (e.g., -1 for PCM mode)
}

/// Rate item
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateItem {
    pub index: u32,
    pub rate: u32,
}

/// Filter item with arg
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilterItem {
    pub index: u32,
    pub name: String,
    pub value: i32, // Filter values can be negative
    pub arg: u32,
}

/// Pipeline settings for a single setting type
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineSetting {
    pub selected: SelectedOption,
    pub options: Vec<SelectOption>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectedOption {
    pub value: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectOption {
    pub value: String,
    pub label: String,
}

/// Full pipeline status
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineStatus {
    pub status: PipelineState,
    pub volume: PipelineVolume,
    pub settings: PipelineSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineState {
    pub state: String,
    pub mode: String,
    pub active_mode: String,
    pub active_filter: String,
    pub active_shaper: String,
    pub active_rate: u32,
    pub convolution: bool,
    pub invert: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineVolume {
    pub value: i32,
    pub min: i32,
    pub max: i32,
    pub is_fixed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PipelineSettings {
    pub mode: PipelineSetting,
    pub filter1x: PipelineSetting,
    #[serde(rename = "filterNx")]
    pub filter_nx: PipelineSetting,
    pub shaper: PipelineSetting,
    /// Dynamic label for shaper field: "Modulator" in DSD mode, "Shaper" in PCM mode
    pub shaper_label: String,
    pub samplerate: PipelineSetting,
}

/// HQPlayer connection status for API
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HqpConnectionStatus {
    pub connected: bool,
    pub host: Option<String>,
    pub port: u16,
    pub web_port: u16,
    pub info: Option<HqpInfo>,
}

/// Internal connection state
struct HqpConnection {
    stream: BufReader<tokio::net::tcp::OwnedReadHalf>,
    write_half: tokio::net::tcp::OwnedWriteHalf,
}

/// Profile info from web UI
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HqpProfile {
    pub value: String,
    pub title: String,
}

/// Matrix profile info
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatrixProfile {
    pub index: u32,
    pub name: String,
}

/// Internal adapter state
#[allow(dead_code)]
struct HqpAdapterState {
    instance_name: Option<String>,
    host: Option<String>,
    port: u16,
    web_port: u16,
    web_username: Option<String>,
    web_password: Option<String>,
    connected: bool,
    info: Option<HqpInfo>,
    last_state: Option<HqpState>,
    modes: Vec<ListItem>,
    filters: Vec<FilterItem>,
    shapers: Vec<ListItem>,
    rates: Vec<RateItem>,
    volume_range: Option<VolumeRange>,
    // Web client state for profiles
    profiles: Vec<HqpProfile>,
    hidden_fields: HashMap<String, String>,
    config_title: Option<String>,
    digest_auth: Option<DigestAuth>,
    cookies: HashMap<String, String>,
    /// Injectable timeout/retry policy. Defaults to the shipped constants.
    timeouts: HqpTimeouts,
}

/// Digest authentication state
struct DigestAuth {
    realm: String,
    nonce: String,
    qop: String,
    opaque: String,
    algorithm: String,
    nc: u32,
}

impl Default for HqpAdapterState {
    fn default() -> Self {
        Self {
            instance_name: None,
            host: None,
            port: DEFAULT_PORT,
            web_port: DEFAULT_WEB_PORT,
            web_username: None,
            web_password: None,
            connected: false,
            info: None,
            last_state: None,
            modes: Vec::new(),
            filters: Vec::new(),
            shapers: Vec::new(),
            rates: Vec::new(),
            volume_range: None,
            profiles: Vec::new(),
            hidden_fields: HashMap::new(),
            config_title: None,
            digest_auth: None,
            cookies: HashMap::new(),
            timeouts: HqpTimeouts::default(),
        }
    }
}

/// HQPlayer adapter
pub struct HqpAdapter {
    state: Arc<RwLock<HqpAdapterState>>,
    connection: Arc<Mutex<Option<HqpConnection>>>,
    http_client: Client,
    bus: SharedBus,
    /// Unsolicited documents skipped while awaiting command replies. Diagnostics for tier-1 live
    /// verification: against a well-behaved daemon this should stay at zero, so a non-zero count on
    /// real hardware is the signal that the reply-element invariant is narrower than documented.
    unsolicited_skipped: Arc<std::sync::atomic::AtomicU32>,
}

impl HqpAdapter {
    pub fn new(bus: SharedBus) -> Self {
        #[allow(clippy::expect_used)] // HTTP client creation only fails if TLS setup fails
        let http_client = Client::builder()
            .timeout(Duration::from_secs(3))
            .build()
            .expect("Failed to create HTTP client");
        let adapter = Self {
            state: Arc::new(RwLock::new(HqpAdapterState::default())),
            connection: Arc::new(Mutex::new(None)),
            http_client,
            bus,
            unsolicited_skipped: Arc::new(std::sync::atomic::AtomicU32::new(0)),
        };
        // Load saved config synchronously at startup
        adapter.load_config_sync();
        adapter
    }

    /// Load config from disk (sync, for startup)
    fn load_config_sync(&self) {
        let path = hqp_config_path();
        if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(content) => match serde_json::from_str::<SavedHqpConfig>(&content) {
                    Ok(saved) => {
                        if let Ok(mut state) = self.state.try_write() {
                            state.host = Some(saved.host.clone());
                            state.port = saved.port;
                            state.web_port = saved.web_port;
                            state.web_username = saved.username;
                            state.web_password = saved.password;
                            tracing::info!(
                                "Loaded HQPlayer config from disk: {}:{}",
                                saved.host,
                                saved.port
                            );
                        }
                    }
                    Err(e) => tracing::warn!("Failed to parse HQPlayer config: {}", e),
                },
                Err(e) => tracing::warn!("Failed to read HQPlayer config: {}", e),
            }
        }
    }

    /// Save config to disk
    async fn save_config(&self) {
        let state = self.state.read().await;
        if let Some(ref host) = state.host {
            let saved = SavedHqpConfig {
                host: host.clone(),
                port: state.port,
                web_port: state.web_port,
                username: state.web_username.clone(),
                password: state.web_password.clone(),
            };
            let path = hqp_config_path();
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match serde_json::to_string_pretty(&saved) {
                Ok(json) => {
                    if let Err(e) = std::fs::write(&path, json) {
                        tracing::error!("Failed to save HQPlayer config: {}", e);
                    } else {
                        tracing::info!("Saved HQPlayer config to disk");
                    }
                }
                Err(e) => tracing::error!("Failed to serialize HQPlayer config: {}", e),
            }
        }
    }

    /// Configure the HQPlayer connection
    pub async fn configure(
        &self,
        host: String,
        port: Option<u16>,
        web_port: Option<u16>,
        web_username: Option<String>,
        web_password: Option<String>,
    ) {
        let changed = {
            let mut state = self.state.write().await;
            let port = port.unwrap_or(DEFAULT_PORT);

            let changed = state.host.as_ref() != Some(&host) || state.port != port;
            state.host = Some(host);
            state.port = port;
            state.web_port = web_port.unwrap_or(DEFAULT_WEB_PORT);

            // Only update credentials if new values are provided (preserve existing)
            if web_username.is_some() {
                state.web_username = web_username;
            }
            if web_password.is_some() {
                state.web_password = web_password;
            }

            // Reset auth state when reconfiguring
            state.digest_auth = None;
            state.cookies.clear();

            if changed {
                state.connected = false;
                // Clear ALL instance-specific cached data when switching hosts
                state.info = None;
                state.last_state = None;
                state.modes.clear();
                state.filters.clear();
                state.shapers.clear();
                state.rates.clear();
                state.volume_range = None;
                state.profiles.clear();
                state.hidden_fields.clear();
                state.config_title = None;
            }
            changed
        };

        if changed {
            let mut conn = self.connection.lock().await;
            *conn = None;
        }

        // Persist to disk
        self.save_config().await;
    }

    /// Check if web credentials are configured
    pub async fn has_web_credentials(&self) -> bool {
        let state = self.state.read().await;
        state.host.is_some() && state.web_username.is_some() && state.web_password.is_some()
    }

    /// How many unsolicited documents this client has skipped while awaiting command replies.
    ///
    /// Diagnostics for tier-1 live verification: the reply-element invariant says a skip should never
    /// happen against a well-behaved daemon, so a non-zero count on real hardware is the signal that
    /// the invariant does not hold as broadly as the reference implies.
    pub async fn unsolicited_skipped(&self) -> u32 {
        self.unsolicited_skipped
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Current timeout/retry policy.
    pub async fn timeouts(&self) -> HqpTimeouts {
        self.state.read().await.timeouts
    }

    /// Override the timeout/retry policy.
    ///
    /// Internal seam for the conformance suite so timeout and reconnect boundaries can be
    /// exercised without waiting on a wall clock. Not reachable over HTTP and not part of any
    /// serialized payload.
    pub async fn set_timeouts(&self, timeouts: HqpTimeouts) {
        self.state.write().await.timeouts = timeouts;
    }

    /// Check if configured
    pub async fn is_configured(&self) -> bool {
        self.state.read().await.host.is_some()
    }

    /// Get connection status
    pub async fn get_status(&self) -> HqpConnectionStatus {
        let state = self.state.read().await;
        HqpConnectionStatus {
            connected: state.connected,
            host: state.host.clone(),
            port: state.port,
            web_port: state.web_port,
            info: state.info.clone(),
        }
    }

    /// Connect to HQPlayer
    pub async fn connect(&self) -> Result<()> {
        let (host, port) = {
            let state = self.state.read().await;
            let host = state
                .host
                .clone()
                .ok_or_else(|| anyhow!("HQPlayer host not configured"))?;
            (host, state.port)
        };

        let addr = format!("{}:{}", host, port);
        let connect_timeout = self.timeouts().await.connect;
        let stream = timeout(connect_timeout, TcpStream::connect(&addr))
            .await
            .map_err(|_| anyhow!("Connection timeout"))?
            .map_err(|e| anyhow!("Connection failed: {}", e))?;

        let (read_half, write_half) = stream.into_split();
        let reader = BufReader::new(read_half);

        {
            let mut conn = self.connection.lock().await;
            *conn = Some(HqpConnection {
                stream: reader,
                write_half,
            });
        }

        {
            let mut state = self.state.write().await;
            state.connected = true;
        }

        // Minimal on-connect: just GetInfo + Status to verify connection
        let info = self.get_info_inner().await?;
        let status = self.get_playback_status_inner().await.unwrap_or_default();

        {
            let mut state = self.state.write().await;
            state.info = Some(info.clone());
        }

        tracing::info!("HQPlayer connected: {} v{}", info.name, info.version);
        self.bus
            .publish(BusEvent::HqpConnected { host: host.clone() });

        // Get instance name for zone ID
        let instance_name = {
            let state = self.state.read().await;
            state.instance_name.clone()
        };

        // Emit ZoneDiscovered for this HQPlayer instance
        let zone = Self::hqp_status_to_zone(
            &host,
            instance_name.as_deref(),
            &info,
            &status,
            &VolumeRange::default(),
        );
        self.bus.publish(BusEvent::ZoneDiscovered { zone });

        // Lists are fetched lazily on first pipeline request via refresh_lists()
        // (Background fetch was removed due to response desync bugs - it used single-line
        // read_line() which corrupted the TCP buffer when interleaved with multi-line responses)

        Ok(())
    }

    /// Disconnect
    pub async fn disconnect(&self) {
        let (host, instance_name) = {
            let mut state = self.state.write().await;
            state.connected = false;
            (state.host.clone(), state.instance_name.clone())
        };

        {
            let mut conn = self.connection.lock().await;
            *conn = None;
        }

        if let Some(ref h) = host {
            // Emit ZoneRemoved for this HQPlayer instance
            let zone_id = PrefixedZoneId::hqplayer(instance_name.as_deref().unwrap_or(h));
            self.bus.publish(BusEvent::ZoneRemoved { zone_id });

            self.bus
                .publish(BusEvent::HqpDisconnected { host: h.clone() });
        }
    }

    /// Refresh cached lists (modes, filters, shapers, rates)
    /// Call this after profile changes
    async fn refresh_lists(&self) {
        let modes = match self.get_modes().await {
            Ok(m) => {
                tracing::debug!("Fetched {} modes", m.len());
                m
            }
            Err(e) => {
                tracing::warn!("Failed to fetch modes: {}", e);
                Vec::new()
            }
        };
        let filters = match self.get_filters().await {
            Ok(f) => {
                tracing::debug!("Fetched {} filters", f.len());
                f
            }
            Err(e) => {
                tracing::warn!("Failed to fetch filters: {}", e);
                Vec::new()
            }
        };
        let shapers = match self.get_shapers().await {
            Ok(s) => {
                tracing::debug!("Fetched {} shapers", s.len());
                s
            }
            Err(e) => {
                tracing::warn!("Failed to fetch shapers: {}", e);
                Vec::new()
            }
        };
        let rates = match self.get_rates().await {
            Ok(r) => {
                tracing::debug!("Fetched {} rates", r.len());
                r
            }
            Err(e) => {
                tracing::warn!("Failed to fetch rates: {}", e);
                Vec::new()
            }
        };

        let mut state = self.state.write().await;
        state.modes = modes;
        state.filters = filters;
        state.shapers = shapers;
        state.rates = rates;
        tracing::debug!("Refreshed HQPlayer lists cache");
    }

    /// Ensure connection is established, reconnecting if needed
    pub async fn ensure_connected(&self) -> Result<()> {
        // Check if already connected
        {
            let conn = self.connection.lock().await;
            if conn.is_some() {
                return Ok(());
            }
        }

        // Not connected, try to connect
        self.connect().await
    }

    /// Mark connection as broken (called on communication errors)
    async fn mark_disconnected(&self) {
        let (host, instance_name) = {
            let mut state = self.state.write().await;
            state.connected = false;
            (state.host.clone(), state.instance_name.clone())
        };

        {
            let mut conn = self.connection.lock().await;
            *conn = None;
        }

        if let Some(ref h) = host {
            tracing::warn!("HQPlayer connection lost to {}", h);
            // Emit ZoneRemoved for this HQPlayer instance
            let zone_id = PrefixedZoneId::hqplayer(instance_name.as_deref().unwrap_or(h));
            self.bus.publish(BusEvent::ZoneRemoved { zone_id });
        }
    }

    /// Fail when the daemon answered `result="Error"`.
    ///
    /// Setters and transport commands echo the request element with `result="OK"` or
    /// `result="Error"`, the latter carrying a reason as element text. An **absent** `result` is a
    /// third, legitimate case: queries never carry one, and `SetAdaptiveVolume` answers a bare
    /// element. Only an explicit `Error` is a failure.
    ///
    /// This is the no-false-success primitive: `OK` still does not prove the setting applied — see
    /// `verify_applied` — but an explicit rejection can no longer be reported as success.
    fn check_result(response: &str) -> Result<()> {
        match Self::parse_attr(response, "result").as_deref() {
            Some("Error") => {
                let element = framing::root_element(response).unwrap_or_else(|| "?".to_string());
                match framing::root_text(response) {
                    Some(reason) => Err(anyhow!("HQPlayer rejected {}: {}", element, reason)),
                    None => Err(anyhow!("HQPlayer rejected {} (no reason given)", element)),
                }
            }
            _ => Ok(()),
        }
    }

    /// Send command and get response with auto-reconnection
    async fn send_command(&self, xml: &str) -> Result<String> {
        let timeouts = self.timeouts().await;
        let mut last_error = None;

        for attempt in 0..timeouts.max_attempts {
            // Ensure we're connected
            if let Err(e) = self.ensure_connected().await {
                last_error = Some(e);
                if attempt + 1 < timeouts.max_attempts {
                    tracing::debug!(
                        "HQPlayer connection attempt {} failed, retrying...",
                        attempt + 1
                    );
                    tokio::time::sleep(timeouts.reconnect_delay).await;
                }
                continue;
            }

            // Try to send command
            match self.send_command_inner(xml).await {
                Ok(response) => {
                    // An explicit rejection is terminal: retrying an invalid value cannot help,
                    // and the connection is still good.
                    Self::check_result(&response)?;
                    return Ok(response);
                }
                Err(e) => {
                    // Mark as disconnected so next attempt will reconnect
                    self.mark_disconnected().await;
                    last_error = Some(e);

                    if attempt + 1 < timeouts.max_attempts {
                        tracing::debug!(
                            "HQPlayer command failed, reconnecting (attempt {})...",
                            attempt + 1
                        );
                        tokio::time::sleep(timeouts.reconnect_delay).await;
                    }
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow!("Failed to send command after retries")))
    }

    /// Inner send command (without retry logic)
    async fn send_command_inner(&self, xml: &str) -> Result<String> {
        let timeouts = self.timeouts().await;
        let mut conn_guard = self.connection.lock().await;
        let conn = conn_guard
            .as_mut()
            .ok_or_else(|| anyhow!("Not connected"))?;

        // Send command
        conn.write_half.write_all(xml.as_bytes()).await?;
        conn.write_half.write_all(b"\n").await?;
        conn.write_half.flush().await?;

        // The element the daemon must answer with. Setters echo the request element and queries
        // return a container of the same name, so a reply's root element always matches its
        // request's. That invariant is what lets an unsolicited document be skipped rather than
        // mistaken for this command's reply.
        let expected_element = framing::root_element(xml);

        // Read until a complete XML document parses. Documents are newline-terminated but may
        // contain internal newlines, so a line is a read hint and not a frame: stopping at the
        // first `/>` truncates any container with a self-closing child (notably
        // `<Status …><metadata …/></Status>`) and leaves its closing tag in the socket for the next
        // command to consume. See the `framing` module.
        // Accumulated as bytes and read in fixed-size chunks, so the ceiling below is a bound on what
        // is **allocated** and not merely on what is retained. An earlier revision read whole lines
        // and checked afterwards, which bounds nothing: `read_line` grows its target until it finds a
        // newline or the peer closes, so one newline-free line is unbounded however small the cap.
        let mut raw: Vec<u8> = Vec::new();
        let mut chunk = [0u8; RESPONSE_READ_CHUNK];
        let mut complete = false;
        let mut skipped = 0u32;

        // One budget for the whole command. Each read gets what is left of it, so skipping an
        // unsolicited document costs the same as waiting for a wanted one.
        let deadline = tokio::time::Instant::now() + timeouts.response;

        // Two nested loops on purpose. The outer one reads; the inner one drains everything the buffer
        // already holds before the outer one is allowed to read again. Draining one document and going
        // straight back to the socket blocks on bytes that may already be in hand — one read can carry
        // an unsolicited push frame *and* the reply behind it, since the daemon emits those frames of
        // its own accord.
        'reading: while !complete {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(anyhow!("Response timeout"));
            }

            let read_result = timeout(remaining, conn.stream.read(&mut chunk)).await;

            let n = match read_result {
                Ok(Ok(0)) => break, // EOF
                Ok(Ok(n)) => n,
                Ok(Err(e)) => return Err(anyhow!("Read error: {}", e)),
                Err(_) => return Err(anyhow!("Response timeout")),
            };

            // Checked **before** the append, against a fixed-size chunk that has already landed on
            // the stack. Nothing unbounded is ever allocated. Returning an error is enough to
            // recover: `send_command`'s wrapper marks the connection disconnected on any inner
            // failure, so the next command reconnects onto a clean stream rather than reading this
            // reply's tail.
            if raw.len() + n > MAX_RESPONSE_BYTES {
                return Err(anyhow!(
                    "Response exceeded {} bytes awaiting a {} reply; discarding the \
                     connection rather than accumulating further",
                    MAX_RESPONSE_BYTES,
                    expected_element.unwrap_or_default()
                ));
            }
            let fresh = &chunk[..n];
            raw.extend_from_slice(fresh);

            // A newline is a **hint**, not a frame: documents are newline-terminated but may contain
            // internal newlines. Attempting to frame only when one arrives keeps the classification
            // frequency at roughly once per document, as it was when this loop read whole lines.
            // Classifying on every chunk instead would re-parse the whole accumulated buffer per
            // chunk, which is quadratic in a large reply. A reply with no newline at all never frames
            // — and is exactly what the ceiling above stops.
            if !fresh.contains(&b'\n') {
                continue 'reading;
            }

            // Drain every document the buffer already holds before reading again.
            loop {
                // Classify the longest valid UTF-8 prefix. A multi-byte character can straddle a
                // chunk boundary, so the tail of `raw` may be a partial sequence; it is not
                // discarded, only excluded from this attempt until its remaining bytes arrive.
                let response = match std::str::from_utf8(&raw) {
                    Ok(s) => s,
                    Err(e) => match std::str::from_utf8(&raw[..e.valid_up_to()]) {
                        Ok(s) => s,
                        // Unreachable by construction: `valid_up_to` is a valid boundary.
                        Err(_) => continue 'reading,
                    },
                };

                match framing::classify(response) {
                    framing::Framing::Complete => {
                        let got = framing::root_element(response);
                        if expected_element.is_some() && got != expected_element {
                            // An unsolicited document — the daemon emits `Status` push frames of its
                            // own accord. Drop it and look at what is left, rather than answering
                            // from someone else's document *or* going back to the socket for a reply
                            // that may already be sitting behind it.
                            skipped += 1;
                            self.unsolicited_skipped
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            if skipped > MAX_UNSOLICITED_BACKLOG {
                                return Err(anyhow!(
                                    "Gave up after {} unsolicited documents while awaiting a {} \
                                     reply (last was {:?})",
                                    skipped,
                                    expected_element.unwrap_or_default(),
                                    got
                                ));
                            }
                            tracing::debug!(
                                "Skipping unsolicited HQPlayer {:?} document while awaiting {:?}",
                                got,
                                expected_element
                            );
                            // Drop only the skipped document, never the whole buffer, then look
                            // again at the remainder. Falling back to a clear when the boundary is
                            // unknown is unreachable — `Complete` is what got us here — and is
                            // written as a total match rather than an unwrap.
                            match framing::first_document_end(response) {
                                Some(end) => {
                                    raw.drain(..end);
                                    // Nothing left but whitespace: the reply has not arrived yet.
                                    if std::str::from_utf8(&raw)
                                        .map(|r| r.trim().is_empty())
                                        .unwrap_or(false)
                                    {
                                        continue 'reading;
                                    }
                                    continue;
                                }
                                None => {
                                    raw.clear();
                                    continue 'reading;
                                }
                            }
                        }

                        // The wanted reply. Anything coalesced *behind* it in the same read is an
                        // unsolicited follower: count it and drop it here rather than leaving it in
                        // the stream, where it is the right *element* for a later command of the same
                        // name and could be handed over as that command's reply.
                        if let Some(end) = framing::first_document_end(response) {
                            let mut rest = &response[end..];
                            while let Some(next) = framing::first_document_end(rest) {
                                skipped += 1;
                                self.unsolicited_skipped
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                tracing::debug!(
                                    "Dropping unsolicited HQPlayer document coalesced behind a {:?} \
                                     reply",
                                    expected_element
                                );
                                rest = &rest[next..];
                            }
                            raw.truncate(end);
                        }
                        complete = true;
                        break;
                    }
                    framing::Framing::Malformed => {
                        return Err(anyhow!(
                            "Malformed response: {}",
                            response.trim().chars().take(120).collect::<String>()
                        ))
                    }
                    // More bytes needed for this document; go back to the socket.
                    framing::Framing::Incomplete => continue 'reading,
                }
            }
        }

        if !complete {
            return Err(anyhow!(
                "Connection closed mid-document after {} bytes",
                raw.len()
            ));
        }

        // Lossless: the loop only completes on a buffer whose valid UTF-8 prefix framed a document, so
        // any trailing partial sequence belongs to a coalesced follower and not to this reply.
        Ok(String::from_utf8_lossy(&raw).trim().to_string())
    }

    // =========================================================================
    // Inner methods (used during connect, no auto-reconnect to avoid recursion)
    // =========================================================================

    /// Get HQPlayer info (no reconnection)
    async fn get_info_inner(&self) -> Result<HqpInfo> {
        let xml = Self::build_request("GetInfo", &[]);
        let response = self.send_command_inner(&xml).await?;

        Ok(HqpInfo {
            name: Self::parse_attr(&response, "name").unwrap_or_default(),
            product: Self::parse_attr(&response, "product").unwrap_or_default(),
            version: Self::parse_attr(&response, "version").unwrap_or_default(),
            platform: Self::parse_attr(&response, "platform").unwrap_or_default(),
            engine: Self::parse_attr(&response, "engine").unwrap_or_default(),
        })
    }

    /// Get playback status (no reconnection)
    async fn get_playback_status_inner(&self) -> Result<HqpStatus> {
        let xml = Self::build_request("Status", &[("subscribe", "0")]);
        let response = self.send_command_inner(&xml).await?;

        Ok(HqpStatus {
            state: Self::parse_attr_u32(&response, "state") as u8,
            track: Self::parse_attr_u32(&response, "track"),
            track_id: Self::parse_attr(&response, "track_id").unwrap_or_default(),
            position: Self::parse_attr_u32(&response, "position"),
            length: Self::parse_attr_u32(&response, "length"),
            volume: Self::parse_attr_i32(&response, "volume"),
            volume_db: Self::parse_attr_f64(&response, "volume").unwrap_or_default(),
            active_mode: Self::parse_attr(&response, "active_mode").unwrap_or_default(),
            active_filter: Self::parse_attr(&response, "active_filter").unwrap_or_default(),
            active_shaper: Self::parse_attr(&response, "active_shaper").unwrap_or_default(),
            active_rate: Self::parse_attr_u32(&response, "active_rate"),
            active_bits: Self::parse_attr_u32(&response, "active_bits"),
            active_channels: Self::parse_attr_u32(&response, "active_channels"),
            samplerate: Self::parse_attr_u32(&response, "samplerate"),
            bitrate: Self::parse_attr_u32(&response, "bitrate"),
        })
    }

    /// Build XML request
    #[allow(clippy::unwrap_used)] // XML writer to Vec and UTF-8 conversion cannot fail
    fn build_request(element: &str, attrs: &[(&str, &str)]) -> String {
        let mut writer = Writer::new(Cursor::new(Vec::new()));

        let mut elem = BytesStart::new(element);
        for (key, value) in attrs {
            elem.push_attribute((*key, *value));
        }

        writer.write_event(Event::Empty(elem)).unwrap();

        format!(
            "<?xml version=\"1.0\"?>{}",
            String::from_utf8(writer.into_inner().into_inner()).unwrap()
        )
    }

    /// Parse an attribute off a response's **root element**.
    ///
    /// Scoped to the root opening tag for two reasons. A whole-document scan matches the XML
    /// declaration's `version="1.0"` before `<GetInfo … version="6"/>`, and it can also pick up a
    /// child element's attribute (a `Status` document's `<metadata … samplerate="…"/>`) in
    /// preference to the root's own. The leading space still guards against matching a longer
    /// attribute name's suffix, e.g. `mode` inside `active_mode`.
    fn parse_attr(xml: &str, attr: &str) -> Option<String> {
        let scope = framing::root_open_tag(xml).unwrap_or(xml);
        let pattern = format!(" {}=\"", attr);
        let start = scope.find(&pattern)? + pattern.len();
        let rest = &scope[start..];
        let end = rest.find('"')?;
        Some(framing::decode_entities(&rest[..end]))
    }

    /// Parse an attribute that the daemon sends as a double, e.g. any dB value.
    ///
    /// Accepts the integer form too, since the daemon sends `-60` and `-60.0` interchangeably.
    fn parse_attr_f64(xml: &str, attr: &str) -> Option<f64> {
        Self::parse_attr(xml, attr).and_then(|s| s.trim().parse().ok())
    }

    /// Integer view of an attribute the daemon may send as a double.
    ///
    /// `"-23.5".parse::<i32>()` fails, and the old `unwrap_or(0)` turned a quiet -23.5 dB into
    /// 0 dB, i.e. maximum output. Rounding the double is the only safe reading.
    ///
    /// The unsigned sibling below clamps a negative double to 0. That is deliberate but narrow: no
    /// documented `u32` attribute is signed or decimal, so a negative there is anomalous either way,
    /// and both the clamp and the `unwrap_or(0)` fallback land on the same value. It is called out
    /// because silently-wrong defaults are the bug class this whole boundary exists to remove — if a
    /// signed `u32`-shaped attribute ever turns up, this needs to become an error rather than a 0.
    fn parse_attr_i32(xml: &str, attr: &str) -> i32 {
        Self::parse_attr(xml, attr)
            .and_then(|s| {
                s.parse::<i32>()
                    .ok()
                    .or_else(|| s.parse::<f64>().ok().map(|f| f.round() as i32))
            })
            .unwrap_or(0)
    }

    fn parse_attr_u32(xml: &str, attr: &str) -> u32 {
        Self::parse_attr(xml, attr)
            .and_then(|s| {
                s.parse::<u32>()
                    .ok()
                    .or_else(|| s.parse::<f64>().ok().map(|f| f.round().max(0.0) as u32))
            })
            .unwrap_or(0)
    }

    fn parse_attr_bool(xml: &str, attr: &str) -> bool {
        Self::parse_attr(xml, attr)
            .map(|s| s == "1")
            .unwrap_or(false)
    }

    /// Get HQPlayer info
    pub async fn get_info(&self) -> Result<HqpInfo> {
        let xml = Self::build_request("GetInfo", &[]);
        let response = self.send_command(&xml).await?;

        Ok(HqpInfo {
            name: Self::parse_attr(&response, "name").unwrap_or_default(),
            product: Self::parse_attr(&response, "product").unwrap_or_default(),
            version: Self::parse_attr(&response, "version").unwrap_or_default(),
            platform: Self::parse_attr(&response, "platform").unwrap_or_default(),
            engine: Self::parse_attr(&response, "engine").unwrap_or_default(),
        })
    }

    /// Get current state
    pub async fn get_state(&self) -> Result<HqpState> {
        let xml = Self::build_request("State", &[]);
        let response = self.send_command(&xml).await?;

        Ok(HqpState {
            state: Self::parse_attr_u32(&response, "state") as u8,
            mode: Self::parse_attr_u32(&response, "mode") as u8,
            filter: Self::parse_attr_u32(&response, "filter"),
            filter1x: Self::parse_attr(&response, "filter1x").and_then(|s| s.parse().ok()),
            filter_nx: Self::parse_attr(&response, "filterNx").and_then(|s| s.parse().ok()),
            shaper: Self::parse_attr_u32(&response, "shaper"),
            rate: Self::parse_attr_u32(&response, "rate"),
            volume: Self::parse_attr_i32(&response, "volume"),
            volume_db: Self::parse_attr_f64(&response, "volume").unwrap_or_default(),
            filter_junk: Self::parse_attr_u32(&response, "filter_junk"),
            active_mode: Self::parse_attr_u32(&response, "active_mode") as u8,
            active_rate: Self::parse_attr_u32(&response, "active_rate"),
            invert: Self::parse_attr_bool(&response, "invert"),
            convolution: Self::parse_attr_bool(&response, "convolution"),
            repeat: Self::parse_attr_u32(&response, "repeat") as u8,
            random: Self::parse_attr_bool(&response, "random"),
            adaptive: Self::parse_attr_bool(&response, "adaptive"),
            filter_20k: Self::parse_attr_bool(&response, "filter_20k"),
            matrix_profile: Self::parse_attr(&response, "matrix_profile").unwrap_or_default(),
        })
    }

    /// Get playback status
    pub async fn get_playback_status(&self) -> Result<HqpStatus> {
        let xml = Self::build_request("Status", &[("subscribe", "0")]);
        let response = self.send_command(&xml).await?;

        Ok(HqpStatus {
            state: Self::parse_attr_u32(&response, "state") as u8,
            track: Self::parse_attr_u32(&response, "track"),
            track_id: Self::parse_attr(&response, "track_id").unwrap_or_default(),
            position: Self::parse_attr_u32(&response, "position"),
            length: Self::parse_attr_u32(&response, "length"),
            volume: Self::parse_attr_i32(&response, "volume"),
            volume_db: Self::parse_attr_f64(&response, "volume").unwrap_or_default(),
            active_mode: Self::parse_attr(&response, "active_mode").unwrap_or_default(),
            active_filter: Self::parse_attr(&response, "active_filter").unwrap_or_default(),
            active_shaper: Self::parse_attr(&response, "active_shaper").unwrap_or_default(),
            active_rate: Self::parse_attr_u32(&response, "active_rate"),
            active_bits: Self::parse_attr_u32(&response, "active_bits"),
            active_channels: Self::parse_attr_u32(&response, "active_channels"),
            samplerate: Self::parse_attr_u32(&response, "samplerate"),
            bitrate: Self::parse_attr_u32(&response, "bitrate"),
        })
    }

    /// Get volume range
    pub async fn get_volume_range(&self) -> Result<VolumeRange> {
        let xml = Self::build_request("VolumeRange", &[]);
        let response = self.send_command(&xml).await?;

        Ok(VolumeRange {
            min: Self::parse_attr_i32(&response, "min"),
            max: Self::parse_attr_i32(&response, "max"),
            step: Self::parse_attr_i32(&response, "step").max(1),
            enabled: Self::parse_attr_bool(&response, "enabled"),
            adaptive: Self::parse_attr_bool(&response, "adaptive"),
            min_db: Self::parse_attr_f64(&response, "min").unwrap_or_default(),
            max_db: Self::parse_attr_f64(&response, "max").unwrap_or_default(),
            step_db: Self::parse_attr_f64(&response, "step"),
        })
    }

    /// Parse multi-item response
    fn parse_items<F, T>(response: &str, item_tag: &str, parser: F) -> Vec<T>
    where
        F: Fn(&str) -> T,
    {
        let mut items = Vec::new();
        let pattern = format!("<{}", item_tag);

        for part in response.split(&pattern).skip(1) {
            if let Some(end) = part.find("/>") {
                let item_xml = format!("<{}{}", item_tag, &part[..end + 2]);
                items.push(parser(&item_xml));
            }
        }

        items
    }

    /// Get available modes
    pub async fn get_modes(&self) -> Result<Vec<ListItem>> {
        let xml = Self::build_request("GetModes", &[]);
        let response = self.send_command(&xml).await?;

        Ok(Self::parse_items(&response, "ModesItem", |item| ListItem {
            index: Self::parse_attr_u32(item, "index"),
            name: Self::parse_attr(item, "name").unwrap_or_default(),
            value: Self::parse_attr_i32(item, "value"), // Mode values can be negative (-1 for PCM)
        }))
    }

    /// Get available filters
    pub async fn get_filters(&self) -> Result<Vec<FilterItem>> {
        let xml = Self::build_request("GetFilters", &[]);
        let response = self.send_command(&xml).await?;

        let filters = Self::parse_items(&response, "FiltersItem", |item| FilterItem {
            index: Self::parse_attr_u32(item, "index"),
            name: Self::parse_attr(item, "name").unwrap_or_default(),
            value: Self::parse_attr_i32(item, "value"),
            arg: Self::parse_attr_u32(item, "arg"),
        });

        // Log first 10 filters to help debug index vs value issues
        for (i, f) in filters.iter().take(10).enumerate() {
            tracing::debug!(
                "Filter[{}]: index={}, name='{}', value={}",
                i,
                f.index,
                f.name,
                f.value
            );
        }
        if filters.len() > 10 {
            tracing::debug!("... and {} more filters", filters.len() - 10);
        }

        Ok(filters)
    }

    /// Get available shapers
    pub async fn get_shapers(&self) -> Result<Vec<ListItem>> {
        let xml = Self::build_request("GetShapers", &[]);
        let response = self.send_command(&xml).await?;

        let shapers = Self::parse_items(&response, "ShapersItem", |item| ListItem {
            index: Self::parse_attr_u32(item, "index"),
            name: Self::parse_attr(item, "name").unwrap_or_default(),
            value: Self::parse_attr_i32(item, "value"),
        });

        // Log first 10 shapers to compare index vs value with filters
        for (i, s) in shapers.iter().take(10).enumerate() {
            tracing::debug!(
                "Shaper[{}]: index={}, name='{}', value={}",
                i,
                s.index,
                s.name,
                s.value
            );
        }
        if shapers.len() > 10 {
            tracing::debug!("... and {} more shapers", shapers.len() - 10);
        }

        Ok(shapers)
    }

    /// Get the 20 kHz "junk" filter list.
    ///
    /// `State.filter_junk` is an int index into this list, not a boolean. The wire element is
    /// `GetJunkFilters` (the CLI advertises `--set-20kfilter` but only accepts `--set-junkfilter`).
    pub async fn get_junk_filters(&self) -> Result<Vec<ListItem>> {
        let xml = Self::build_request("GetJunkFilters", &[]);
        let response = self.send_command(&xml).await?;
        Ok(Self::parse_items(&response, "JunkFiltersItem", |item| {
            ListItem {
                index: Self::parse_attr_u32(item, "index"),
                name: Self::parse_attr(item, "name").unwrap_or_default(),
                value: Self::parse_attr_i32(item, "value"),
            }
        }))
    }

    /// Get available sample rates
    pub async fn get_rates(&self) -> Result<Vec<RateItem>> {
        let xml = Self::build_request("GetRates", &[]);
        let response = self.send_command(&xml).await?;

        Ok(Self::parse_items(&response, "RatesItem", |item| RateItem {
            index: Self::parse_attr_u32(item, "index"),
            rate: Self::parse_attr_u32(item, "rate"),
        }))
    }

    /// Confirm a setting actually applied, by reading `State` back.
    ///
    /// Verified daemon behaviour: "a setter can return `result=\"OK\"` without the setting actually
    /// applying. Never trust `result=\"OK\"` alone; confirm via `State` readback." A change can also
    /// land a poll later than the acknowledgement, so this polls rather than checking once, reusing
    /// the injected retry policy instead of introducing another knob.
    ///
    /// Returns an error when the setting never appears, so a caller can never be told about a change
    /// that did not happen.
    async fn verify_applied<F>(&self, what: &str, expected_index: u32, matches: F) -> Result<()>
    where
        F: Fn(&HqpState) -> Option<u32>,
    {
        let timeouts = self.timeouts().await;
        let mut last_seen = None;

        for attempt in 0..timeouts.max_attempts.max(1) {
            if attempt > 0 {
                tokio::time::sleep(timeouts.reconnect_delay).await;
            }
            let state = self.get_state().await?;
            let seen = matches(&state);
            if seen == Some(expected_index) {
                return Ok(());
            }
            last_seen = seen;
        }

        Err(anyhow!(
            "HQPlayer accepted {} but {} still reads {} instead of {}; refusing to report an \
             unverified change",
            what,
            what,
            last_seen
                .map(|v| v.to_string())
                .unwrap_or_else(|| "nothing".to_string()),
            expected_index
        ))
    }

    /// Set mode by name (e.g., "PCM", "DSD", "[source]")
    /// Resolves name to INDEX and sends to HQPlayer.
    /// CLI confirms: `--set-mode <index>`
    ///
    /// NOTE: Mode changes affect available filters, shapers, and rates.
    /// We refresh the cached lists after changing mode.
    pub async fn set_mode(&self, mode_name: &str) -> Result<()> {
        let mode_index = self.resolve_mode_index(mode_name).await?;
        let xml = Self::build_request("SetMode", &[("value", &mode_index.to_string())]);
        self.send_command(&xml).await?;
        self.verify_applied("mode", mode_index, |s| Some(u32::from(s.mode)))
            .await?;
        // Mode change affects available filters/shapers/rates - refresh lists
        self.refresh_lists().await;
        Ok(())
    }

    /// Resolve mode name to INDEX, checking cache first then fetching if needed
    /// ModesItem has: index (0,1,2), name, value (-1,0,1) - index ≠ value!
    async fn resolve_mode_index(&self, mode_name: &str) -> Result<u32> {
        // First try to find by name in cached modes (case-insensitive)
        let cached_index = {
            let state = self.state.read().await;
            state
                .modes
                .iter()
                .find(|m| m.name.eq_ignore_ascii_case(mode_name))
                .map(|m| m.index)
        };

        if let Some(idx) = cached_index {
            return Ok(idx);
        }

        // Not found in cache - try parsing as integer (direct index)
        if let Ok(idx) = mode_name.parse::<u32>() {
            return Ok(idx);
        }

        // Try refreshing mode list and searching again
        let modes = self.get_modes().await?;
        modes
            .iter()
            .find(|m| m.name.eq_ignore_ascii_case(mode_name))
            .map(|m| m.index)
            .ok_or_else(|| {
                let available: Vec<_> = modes.iter().map(|m| m.name.as_str()).collect();
                anyhow::anyhow!(
                    "Mode '{}' not found. Available: {}",
                    mode_name,
                    available.join(", ")
                )
            })
    }

    /// Set filter (low-level) - takes INDEX values
    ///
    /// - value: sets the Nx (non-1x) filter by INDEX
    /// - value1x: if provided, also sets the 1x filter by INDEX
    ///
    /// HQPlayer CLI confirms: `--set-filter <index> [index1x]`
    /// State returns INDEX for filter fields, so read from State and send back unchanged.
    pub async fn set_filter(&self, value: u32, value1x: Option<u32>) -> Result<()> {
        let value_str = value.to_string();
        let mut attrs = vec![("value", value_str.as_str())];

        let value1x_str;
        if let Some(v1x) = value1x {
            value1x_str = v1x.to_string();
            attrs.push(("value1x", value1x_str.as_str()));
        }

        let xml = Self::build_request("SetFilter", &attrs);
        tracing::debug!(
            "SetFilter: value={} (Nx), value1x={:?} (1x) | XML: {}",
            value,
            value1x,
            xml.trim()
        );
        self.send_command(&xml).await?;
        Ok(())
    }

    /// Set only the 1x filter, preserving current Nx filter index
    /// Accepts filter name (e.g., "poly-sinc-lp") or index as string
    pub async fn set_filter_1x(&self, filter_name: &str) -> Result<()> {
        let filter_index = self.resolve_filter_index(filter_name).await?;
        let state = self.get_state().await?;
        // State returns INDEX for filters
        let current_nx_index = state.filter_nx.unwrap_or(state.filter);
        tracing::debug!(
            "set_filter_1x: name='{}' resolved_index={}, state.filter_nx={:?}, state.filter={}, using current_nx={}",
            filter_name, filter_index, state.filter_nx, state.filter, current_nx_index
        );
        self.set_filter(current_nx_index, Some(filter_index))
            .await?;
        self.verify_applied("filter1x", filter_index, |s| s.filter1x.or(Some(s.filter)))
            .await
    }

    /// Set only the Nx filter, preserving current 1x filter index
    /// Accepts filter name (e.g., "poly-sinc-lp") or index as string
    pub async fn set_filter_nx(&self, filter_name: &str) -> Result<()> {
        let filter_index = self.resolve_filter_index(filter_name).await?;
        // State returns INDEX for filters
        let state = self.get_state().await?;
        let current_1x_index = state.filter1x.unwrap_or(state.filter);
        tracing::debug!(
            "set_filter_nx: name='{}' resolved_index={}, state.filter1x={:?}, state.filter={}, using current_1x={}",
            filter_name, filter_index, state.filter1x, state.filter, current_1x_index
        );
        self.set_filter(filter_index, Some(current_1x_index))
            .await?;
        self.verify_applied("filterNx", filter_index, |s| s.filter_nx.or(Some(s.filter)))
            .await
    }

    /// Resolve filter name to INDEX, checking cache first then fetching if needed
    async fn resolve_filter_index(&self, filter_name: &str) -> Result<u32> {
        // First try to find by name in cached filters
        let cached_result = {
            let state = self.state.read().await;
            state
                .filters
                .iter()
                .find(|f| f.name == filter_name)
                .map(|f| (f.index, f.value))
        };

        if let Some((idx, val)) = cached_result {
            tracing::debug!(
                "resolve_filter_index: '{}' -> index={}, value={} (from cache)",
                filter_name,
                idx,
                val
            );
            return Ok(idx);
        }

        // Not found in cache - try parsing as integer (direct index)
        if let Ok(idx) = filter_name.parse::<u32>() {
            tracing::debug!(
                "resolve_filter_index: '{}' parsed as direct index={}",
                filter_name,
                idx
            );
            return Ok(idx);
        }

        // Try refreshing filter list and searching again
        let filters = self.get_filters().await?;
        let result = filters
            .iter()
            .find(|f| f.name == filter_name)
            .map(|f| (f.index, f.value));

        match result {
            Some((idx, val)) => {
                tracing::debug!(
                    "resolve_filter_index: '{}' -> index={}, value={} (after refresh)",
                    filter_name,
                    idx,
                    val
                );
                Ok(idx)
            }
            None => Err(anyhow::anyhow!(
                "Filter '{}' not found in available filters",
                filter_name
            )),
        }
    }

    /// Set shaper - sends INDEX to HQPlayer
    /// Accepts shaper name (e.g., "ASDM7") or index as string
    /// HQPlayer CLI confirms: `--set-shaping <index>`
    pub async fn set_shaper(&self, shaper_name: &str) -> Result<()> {
        let shaper_index = self.resolve_shaper_index(shaper_name).await?;
        let xml = Self::build_request("SetShaping", &[("value", &shaper_index.to_string())]);
        self.send_command(&xml).await?;
        self.verify_applied("shaper", shaper_index, |s| Some(s.shaper))
            .await
    }

    /// Resolve shaper name to INDEX, checking cache first then fetching if needed
    async fn resolve_shaper_index(&self, shaper_name: &str) -> Result<u32> {
        // First try to find by name in cached shapers
        let cached_index = {
            let state = self.state.read().await;
            state
                .shapers
                .iter()
                .find(|s| s.name == shaper_name)
                .map(|s| s.index)
        };

        if let Some(idx) = cached_index {
            return Ok(idx);
        }

        // Not found in cache - try parsing as integer (direct index)
        if let Ok(idx) = shaper_name.parse::<u32>() {
            return Ok(idx);
        }

        // Try refreshing shaper list and searching again
        let shapers = self.get_shapers().await?;
        shapers
            .iter()
            .find(|s| s.name == shaper_name)
            .map(|s| s.index)
            .ok_or_else(|| {
                anyhow::anyhow!("Shaper '{}' not found in available shapers", shaper_name)
            })
    }

    /// Set sample rate
    /// The value parameter is the actual rate (e.g., 48000), but HQPlayer's SetRate
    /// command expects the index from the rates list, so we look it up.
    pub async fn set_rate(&self, rate_value: u32) -> Result<()> {
        // Look up the index for this rate value from cached rates
        let index = {
            let state = self.state.read().await;
            state
                .rates
                .iter()
                .find(|r| r.rate == rate_value)
                .map(|r| r.index)
        };

        let index = match index {
            Some(idx) => idx,
            None => {
                // Rate not found in cached list - maybe cache is stale, try refreshing
                let rates = self.get_rates().await?;
                rates
                    .iter()
                    .find(|r| r.rate == rate_value)
                    .map(|r| r.index)
                    .ok_or_else(|| {
                        anyhow::anyhow!("Rate {} not found in available rates", rate_value)
                    })?
            }
        };

        let xml = Self::build_request("SetRate", &[("value", &index.to_string())]);
        self.send_command(&xml).await?;
        self.verify_applied("rate", index, |s| Some(s.rate)).await
    }

    /// Set volume in whole dB.
    ///
    /// Kept for the existing `POST /hqplayer/volume` request payload, whose `value` is an integer.
    /// Delegates to [`Self::set_volume_db`], which is the protocol-accurate form.
    pub async fn set_volume(&self, value: i32) -> Result<()> {
        self.set_volume_db(f64::from(value)).await
    }

    /// Set volume in dB, which the daemon accepts as a double.
    ///
    /// Deliberately result-checked but not readback-verified. A fixed-volume daemon answers an
    /// explicit `result="Error"`, which `check_result` already surfaces, and with adaptive volume
    /// engaged the daemon moves the level on its own, so a readback comparison would report a
    /// spurious failure.
    ///
    /// The wire form is `<Volume value="-23.5"/>`; whole numbers are sent without a decimal part so
    /// the request still looks like the reference client's.
    pub async fn set_volume_db(&self, db: f64) -> Result<()> {
        // `Display` for f64 already omits a trailing `.0`, so `-30.0` formats as `-30` and `-23.5`
        // as `-23.5`. An earlier version branched on `trunc()` to force that, which was both
        // redundant and a latent bug: had the guard ever mis-fired it would have sent `-0` for
        // `-0.5`. Pinned by `a_whole_db_volume_is_not_turned_into_a_fraction_on_the_wire`.
        let xml = Self::build_request("Volume", &[("value", &format!("{db}"))]);
        self.send_command(&xml).await?;
        Ok(())
    }

    /// Volume up
    pub async fn volume_up(&self) -> Result<()> {
        let xml = Self::build_request("VolumeUp", &[]);
        self.send_command(&xml).await?;
        Ok(())
    }

    /// Volume down
    pub async fn volume_down(&self) -> Result<()> {
        let xml = Self::build_request("VolumeDown", &[]);
        self.send_command(&xml).await?;
        Ok(())
    }

    /// Mute toggle
    pub async fn volume_mute(&self) -> Result<()> {
        let xml = Self::build_request("VolumeMute", &[]);
        self.send_command(&xml).await?;
        Ok(())
    }

    /// Play
    pub async fn play(&self) -> Result<()> {
        let xml = Self::build_request("Play", &[("last", "0")]);
        self.send_command(&xml).await?;
        Ok(())
    }

    /// Pause
    pub async fn pause(&self) -> Result<()> {
        let xml = Self::build_request("Pause", &[]);
        self.send_command(&xml).await?;
        Ok(())
    }

    /// Stop
    pub async fn stop(&self) -> Result<()> {
        let xml = Self::build_request("Stop", &[]);
        self.send_command(&xml).await?;
        Ok(())
    }

    /// Previous track
    pub async fn previous(&self) -> Result<()> {
        let xml = Self::build_request("Previous", &[]);
        self.send_command(&xml).await?;
        Ok(())
    }

    /// Next track
    pub async fn next(&self) -> Result<()> {
        let xml = Self::build_request("Next", &[]);
        self.send_command(&xml).await?;
        Ok(())
    }

    /// Seek to position
    pub async fn seek(&self, position: u32) -> Result<()> {
        let xml = Self::build_request("Seek", &[("position", &position.to_string())]);
        self.send_command(&xml).await?;
        Ok(())
    }

    /// Control playback
    pub async fn control(&self, action: &str) -> Result<()> {
        match action {
            "play" => self.play().await,
            "pause" => self.pause().await,
            "stop" => self.stop().await,
            "previous" => self.previous().await,
            "next" => self.next().await,
            _ => Err(anyhow!("Unknown action: {}", action)),
        }
    }

    /// Get full pipeline status
    pub async fn get_pipeline_status(&self) -> Result<PipelineStatus> {
        // Core data: State + Status (2 TCP commands)
        let state = self.get_state().await?;
        let playback_status = self.get_playback_status().await.unwrap_or_default();

        // Lazy-load lists if not cached (first request after connect)
        // Check ALL lists - if any are empty, we need to refresh
        let needs_lists = {
            let cached = self.state.read().await;
            cached.modes.is_empty()
                || cached.filters.is_empty()
                || cached.shapers.is_empty()
                || cached.rates.is_empty()
        };
        if needs_lists {
            self.refresh_lists().await;
        }

        // Lazy-load volume range if not cached
        let needs_vol_range = {
            let cached = self.state.read().await;
            cached.volume_range.is_none()
        };
        if needs_vol_range {
            if let Ok(vr) = self.get_volume_range().await {
                let mut cached = self.state.write().await;
                cached.volume_range = Some(vr);
            }
        }

        // Use cached data
        let (modes, filters, shapers, rates, vol_range) = {
            let cached = self.state.read().await;
            (
                cached.modes.clone(),
                cached.filters.clone(),
                cached.shapers.clone(),
                cached.rates.clone(),
                cached.volume_range.clone().unwrap_or_default(),
            )
        };

        // State returns INDEX for filters and shapers (not value!)
        // See hqp-control help: --set-filter <index> [index1x]
        let filter1x_idx = state.filter1x.unwrap_or(state.filter);
        let filter_nx_idx = state.filter_nx.unwrap_or(state.filter);
        let shaper_idx = state.shaper;

        let filter1x_obj = filters.iter().find(|f| f.index == filter1x_idx);
        let filter_nx_obj = filters.iter().find(|f| f.index == filter_nx_idx);
        let shaper_obj = shapers.iter().find(|s| s.index == shaper_idx);

        // State.mode and State.active_mode return INDEX, not VALUE.
        // ModesItem has: index (0,1,2), name, value (-1,0,1)
        // Reference: onModesItem prints "[index] name value"
        let get_mode_by_index = |idx: u8| -> String {
            modes
                .iter()
                .find(|m| m.index == idx as u32)
                .map(|m| m.name.clone())
                .unwrap_or_else(|| format!("Unknown({})", idx))
        };

        let state_str = match state.state {
            0 => "Stopped",
            1 => "Paused",
            2 => "Playing",
            _ => "Unknown",
        };

        Ok(PipelineStatus {
            status: PipelineState {
                state: state_str.to_string(),
                // State.mode and State.active_mode are INDEX (0,1,2) - look up by ModesItem.index
                mode: get_mode_by_index(state.mode),
                // Use State's active_mode (INDEX) - Status's active_mode string is unreliable
                // (shows "[source]" even when actually outputting DSD)
                active_mode: get_mode_by_index(state.active_mode),
                active_filter: playback_status.active_filter.clone(),
                active_shaper: playback_status.active_shaper.clone(),
                active_rate: state.active_rate,
                convolution: state.convolution,
                invert: state.invert,
            },
            volume: PipelineVolume {
                value: state.volume,
                min: vol_range.min,
                max: vol_range.max,
                is_fixed: !vol_range.enabled,
            },
            settings: PipelineSettings {
                mode: PipelineSetting {
                    selected: SelectedOption {
                        // Use name for the value - adapter handles name→value conversion
                        value: get_mode_by_index(state.mode),
                        label: get_mode_by_index(state.mode),
                    },
                    options: modes
                        .iter()
                        .map(|m| SelectOption {
                            value: m.name.clone(), // Send NAME, not value
                            label: m.name.clone(),
                        })
                        .collect(),
                },
                filter1x: PipelineSetting {
                    selected: SelectedOption {
                        // Use name - adapter handles name→index conversion
                        value: filter1x_obj.map(|f| f.name.clone()).unwrap_or_default(),
                        label: filter1x_obj.map(|f| f.name.clone()).unwrap_or_default(),
                    },
                    options: filters
                        .iter()
                        .map(|f| SelectOption {
                            value: f.name.clone(), // Send NAME, not index
                            label: f.name.clone(),
                        })
                        .collect(),
                },
                filter_nx: PipelineSetting {
                    selected: SelectedOption {
                        // Use name - adapter handles name→index conversion
                        value: filter_nx_obj.map(|f| f.name.clone()).unwrap_or_default(),
                        label: filter_nx_obj.map(|f| f.name.clone()).unwrap_or_default(),
                    },
                    options: filters
                        .iter()
                        .map(|f| SelectOption {
                            value: f.name.clone(), // Send NAME, not index
                            label: f.name.clone(),
                        })
                        .collect(),
                },
                shaper: PipelineSetting {
                    selected: SelectedOption {
                        // Use name - adapter handles name→index conversion
                        value: shaper_obj.map(|s| s.name.clone()).unwrap_or_default(),
                        label: shaper_obj.map(|s| s.name.clone()).unwrap_or_default(),
                    },
                    options: shapers
                        .iter()
                        .map(|s| SelectOption {
                            value: s.name.clone(), // Send NAME, not index
                            label: s.name.clone(),
                        })
                        .collect(),
                },
                // In PCM mode, it's called "Shaper"; in DSD/SDM mode, it's "Modulator"
                shaper_label: {
                    let mode_name = get_mode_by_index(state.mode);
                    // PCM mode → "Shaper", DSD/SDM mode → "Modulator"
                    // "[source]" mode depends on source material - default to "Shaper"
                    if mode_name.to_uppercase().contains("SDM")
                        || mode_name.to_uppercase().contains("DSD")
                    {
                        "Modulator".to_string()
                    } else {
                        "Shaper".to_string()
                    }
                },
                samplerate: PipelineSetting {
                    selected: SelectedOption {
                        // state.rate is an INDEX into the rates list, not a rate value
                        // Look up by index to get the actual rate
                        value: rates
                            .iter()
                            .find(|r| r.index == state.rate)
                            .map(|r| r.rate.to_string())
                            .unwrap_or_else(|| state.active_rate.to_string()),
                        label: rates
                            .iter()
                            .find(|r| r.index == state.rate)
                            .map(|r| {
                                if r.rate == 0 {
                                    "Auto".to_string()
                                } else {
                                    r.rate.to_string()
                                }
                            })
                            .unwrap_or_else(|| state.active_rate.to_string()),
                    },
                    options: rates
                        .iter()
                        .map(|r| SelectOption {
                            // Use rate as value (what HQPlayer expects)
                            value: r.rate.to_string(),
                            label: if r.rate == 0 {
                                "Auto".to_string()
                            } else {
                                r.rate.to_string()
                            },
                        })
                        .collect(),
                },
            },
        })
    }

    // =========================================================================
    // Web UI methods for profile loading (HTTP with Digest Auth)
    // =========================================================================

    /// Get web base URL
    async fn web_base_url(&self) -> Result<String> {
        let state = self.state.read().await;
        let host = state
            .host
            .as_ref()
            .ok_or_else(|| anyhow!("HQPlayer host not configured"))?;
        Ok(format!("http://{}:{}", host, state.web_port))
    }

    /// MD5 hash helper
    fn md5_hash(input: &str) -> String {
        format!("{:x}", md5::compute(input.as_bytes()))
    }

    /// Build digest auth header
    async fn build_digest_header(&self, method: &str, uri: &str) -> Option<String> {
        let mut state = self.state.write().await;

        // Extract all values first to avoid borrow conflicts
        let username = state.web_username.clone()?;
        let password = state.web_password.clone()?;

        let digest = state.digest_auth.as_mut()?;

        digest.nc += 1;
        let nc = format!("{:08x}", digest.nc);
        let cnonce = format!("{:016x}", rand::random::<u64>());

        // Clone digest fields we need
        let realm = digest.realm.clone();
        let nonce = digest.nonce.clone();
        let qop = digest.qop.clone();
        let opaque = digest.opaque.clone();
        let algorithm = digest.algorithm.clone();

        let ha1 = if algorithm.to_uppercase() == "MD5-SESS" {
            let initial = Self::md5_hash(&format!("{}:{}:{}", username, realm, password));
            Self::md5_hash(&format!("{}:{}:{}", initial, nonce, cnonce))
        } else {
            Self::md5_hash(&format!("{}:{}:{}", username, realm, password))
        };

        let ha2 = Self::md5_hash(&format!("{}:{}", method, uri));

        let response = if !qop.is_empty() {
            let qop_value = qop.split(',').next().unwrap_or("auth").trim();
            Self::md5_hash(&format!(
                "{}:{}:{}:{}:{}:{}",
                ha1, nonce, nc, cnonce, qop_value, ha2
            ))
        } else {
            Self::md5_hash(&format!("{}:{}:{}", ha1, nonce, ha2))
        };

        let mut parts = vec![
            format!("Digest username=\"{}\"", username),
            format!("realm=\"{}\"", realm),
            format!("nonce=\"{}\"", nonce),
            format!("uri=\"{}\"", uri),
            format!("algorithm={}", algorithm),
            format!("response=\"{}\"", response),
        ];

        if !qop.is_empty() {
            let qop_value = qop.split(',').next().unwrap_or("auth").trim();
            parts.push(format!("qop={}", qop_value));
            parts.push(format!("nc={}", nc));
            parts.push(format!("cnonce=\"{}\"", cnonce));
        }

        if !opaque.is_empty() {
            parts.push(format!("opaque=\"{}\"", opaque));
        }

        Some(parts.join(", "))
    }

    /// Parse WWW-Authenticate header for digest auth
    async fn parse_digest_challenge(&self, header: &str) {
        let mut state = self.state.write().await;

        let challenge = header
            .trim_start_matches("Digest ")
            .trim_start_matches("digest ");
        let mut realm = String::new();
        let mut nonce = String::new();
        let mut qop = String::new();
        let mut opaque = String::new();
        let mut algorithm = "MD5".to_string();

        for part in challenge.split(',') {
            let part = part.trim();
            if let Some(eq_pos) = part.find('=') {
                let key = part[..eq_pos].trim();
                let value = part[eq_pos + 1..].trim().trim_matches('"');

                match key {
                    "realm" => realm = value.to_string(),
                    "nonce" => nonce = value.to_string(),
                    "qop" => qop = value.to_string(),
                    "opaque" => opaque = value.to_string(),
                    "algorithm" => algorithm = value.to_uppercase(),
                    _ => {}
                }
            }
        }

        state.digest_auth = Some(DigestAuth {
            realm,
            nonce,
            qop,
            opaque,
            algorithm,
            nc: 0,
        });
    }

    /// Make authenticated web request
    async fn web_request(&self, path: &str, method: &str, body: Option<&str>) -> Result<String> {
        let base_url = self.web_base_url().await?;
        let url = format!("{}{}", base_url, path);

        // First attempt
        let mut request = match method {
            "POST" => self.http_client.post(&url),
            _ => self.http_client.get(&url),
        };

        if let Some(auth_header) = self.build_digest_header(method, path).await {
            request = request.header("Authorization", auth_header);
        }

        if let Some(b) = body {
            request = request
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(b.to_string());
        }

        let response = request.send().await?;

        // Handle 401 - parse challenge and retry
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            if let Some(auth_header) = response.headers().get("www-authenticate") {
                if let Ok(header_str) = auth_header.to_str() {
                    if header_str.to_lowercase().starts_with("digest") {
                        self.parse_digest_challenge(header_str).await;

                        // Retry with auth
                        let mut request = match method {
                            "POST" => self.http_client.post(&url),
                            _ => self.http_client.get(&url),
                        };

                        if let Some(auth_header) = self.build_digest_header(method, path).await {
                            request = request.header("Authorization", auth_header);
                        }

                        if let Some(b) = body {
                            request = request
                                .header("Content-Type", "application/x-www-form-urlencoded")
                                .body(b.to_string());
                        }

                        let response = request.send().await?;
                        if !response.status().is_success() {
                            return Err(anyhow!("Request failed: {}", response.status()));
                        }
                        return Ok(response.text().await?);
                    }
                }
            }
            return Err(anyhow!("Authentication failed"));
        }

        if !response.status().is_success() {
            return Err(anyhow!("Request failed: {}", response.status()));
        }

        Ok(response.text().await?)
    }

    /// Parse hidden form inputs from HTML
    #[allow(clippy::unwrap_used)] // Regex patterns are compile-time constants
    fn parse_hidden_inputs(html: &str) -> HashMap<String, String> {
        let mut fields = HashMap::new();

        let input_re = Regex::new(r#"<input[^>]*name\s*=\s*["']([^"'>\s]+)["'][^>]*>"#).unwrap();
        let value_re = Regex::new(r#"value\s*=\s*["']([^"']*)["']"#).unwrap();
        let type_re = Regex::new(r#"type\s*=\s*["']([^"']*)["']"#).unwrap();

        for cap in input_re.captures_iter(html) {
            let tag = &cap[0];
            let name = &cap[1];

            let input_type = type_re
                .captures(tag)
                .map(|c| c[1].to_lowercase())
                .unwrap_or_default();

            if input_type == "hidden" || name == "_xsrf" {
                let value = value_re
                    .captures(tag)
                    .map(|c| c[1].to_string())
                    .unwrap_or_default();
                fields.insert(name.to_string(), value);
            }
        }

        fields
    }

    /// Parse profiles from HTML select
    #[allow(clippy::unwrap_used)] // Regex patterns are compile-time constants
    fn parse_profiles_from_html(html: &str) -> Vec<HqpProfile> {
        let mut profiles = Vec::new();

        let select_re =
            Regex::new(r#"<select[^>]*name\s*=\s*["']profile["'][^>]*>([\s\S]*?)</select>"#)
                .unwrap();
        let option_re = Regex::new(r#"<option([^>]*)>([\s\S]*?)</option>"#).unwrap();
        let value_re = Regex::new(r#"value\s*=\s*["']([^"']*)["']"#).unwrap();

        if let Some(select_cap) = select_re.captures(html) {
            let content = &select_cap[1];

            for opt_cap in option_re.captures_iter(content) {
                let attrs = &opt_cap[1];
                let text = opt_cap[2].trim();

                let value = value_re
                    .captures(attrs)
                    .map(|c| c[1].to_string())
                    .unwrap_or_else(|| text.to_string());

                // Skip default/empty profiles
                let slug: String = value
                    .to_lowercase()
                    .chars()
                    .filter(|c| c.is_alphanumeric())
                    .collect();
                if !value.is_empty() && !slug.is_empty() && slug != "default" {
                    profiles.push(HqpProfile {
                        value: value.trim().to_string(),
                        title: if text.is_empty() {
                            value.clone()
                        } else {
                            text.to_string()
                        },
                    });
                }
            }
        }

        profiles
    }

    /// Read the daemon's `/config` page verbatim.
    ///
    /// A narrow read-only seam over the existing digest path, so tier-1 verification can observe the
    /// persistent lane's *shape* — which form fields exist, whether `[default]` is offered — rather
    /// than only the values the profile parser happens to keep.
    ///
    /// The path is fixed rather than a parameter, deliberately: an arbitrary-path `GET` would be a
    /// general-purpose escape hatch around the higher-level methods, and the narrowness this comment
    /// claims should be enforced by the signature instead of by callers remembering it. `GET` only, so
    /// no write route is reachable. Not an HTTP endpoint of ours and not part of any serialized payload.
    pub async fn fetch_config_page_raw(&self) -> Result<String> {
        self.web_request("/config", "GET", None).await
    }

    /// Fetch available profiles from web UI
    pub async fn fetch_profiles(&self) -> Result<Vec<HqpProfile>> {
        if !self.has_web_credentials().await {
            return Err(anyhow!("Web credentials not configured"));
        }

        let html = self.web_request(PROFILE_PATH, "GET", None).await?;

        let hidden_fields = Self::parse_hidden_inputs(&html);
        let profiles = Self::parse_profiles_from_html(&html);

        // Cache for later use
        {
            let mut state = self.state.write().await;
            state.hidden_fields = hidden_fields;
            state.profiles = profiles.clone();
        }

        Ok(profiles)
    }

    /// Get cached profiles
    pub async fn get_cached_profiles(&self) -> Vec<HqpProfile> {
        self.state.read().await.profiles.clone()
    }

    /// Load a profile via web UI form submission
    pub async fn load_profile(&self, profile_value: &str) -> Result<()> {
        if profile_value.is_empty() || profile_value.to_lowercase() == "default" {
            return Err(anyhow!("Profile value is required"));
        }

        if !self.has_web_credentials().await {
            return Err(anyhow!("Web credentials not configured"));
        }

        // Ensure we have hidden fields
        {
            let state = self.state.read().await;
            if state.hidden_fields.is_empty() || state.profiles.is_empty() {
                drop(state);
                self.fetch_profiles().await?;
            }
        }

        // Build form body
        let body = {
            let state = self.state.read().await;
            let mut params: Vec<(String, String)> = state
                .hidden_fields
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            params.push(("profile".to_string(), profile_value.to_string()));

            params
                .iter()
                .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
                .collect::<Vec<_>>()
                .join("&")
        };

        let base_url = self.web_base_url().await?;

        // POST with proper headers
        let mut request = self
            .http_client
            .post(format!("{}{}", base_url, PROFILE_PATH))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Origin", &base_url)
            .header("Referer", &format!("{}{}", base_url, PROFILE_PATH));

        if let Some(auth_header) = self.build_digest_header("POST", PROFILE_PATH).await {
            request = request.header("Authorization", auth_header);
        }

        let response = request.body(body.clone()).send().await?;

        // Handle 401 retry
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            if let Some(auth_header) = response.headers().get("www-authenticate") {
                if let Ok(header_str) = auth_header.to_str() {
                    self.parse_digest_challenge(header_str).await;

                    let mut request = self
                        .http_client
                        .post(format!("{}{}", base_url, PROFILE_PATH))
                        .header("Content-Type", "application/x-www-form-urlencoded")
                        .header("Origin", &base_url)
                        .header("Referer", &format!("{}{}", base_url, PROFILE_PATH));

                    if let Some(auth_header) = self.build_digest_header("POST", PROFILE_PATH).await
                    {
                        request = request.header("Authorization", auth_header);
                    }

                    let response = request.body(body).send().await?;
                    if response.status().is_client_error() || response.status().is_server_error() {
                        return Err(anyhow!("Profile load failed: {}", response.status()));
                    }
                    // Refresh cached lists after profile change
                    self.refresh_lists().await;
                    return Ok(());
                }
            }
            return Err(anyhow!("Authentication failed"));
        }

        if response.status().is_client_error() || response.status().is_server_error() {
            return Err(anyhow!("Profile load failed: {}", response.status()));
        }

        // Refresh cached lists after profile change
        self.refresh_lists().await;
        Ok(())
    }

    /// Check if this is HQPlayer Embedded (supports profiles)
    pub async fn is_embedded(&self) -> bool {
        let state = self.state.read().await;
        state
            .info
            .as_ref()
            .map(|i| i.product.to_lowercase().contains("embedded"))
            .unwrap_or(false)
    }

    /// Check if profiles are supported (Embedded + web creds)
    pub async fn supports_profiles(&self) -> bool {
        self.is_embedded().await && self.has_web_credentials().await
    }

    // =========================================================================
    // Matrix profile methods (native TCP protocol)
    // =========================================================================

    /// Get available matrix profiles
    pub async fn get_matrix_profiles(&self) -> Result<Vec<MatrixProfile>> {
        let xml = Self::build_request("MatrixListProfiles", &[]);
        let response = self.send_command(&xml).await?;

        Ok(Self::parse_items(&response, "MatrixProfile", |item| {
            MatrixProfile {
                index: Self::parse_attr_u32(item, "index"),
                name: Self::parse_attr(item, "name").unwrap_or_default(),
            }
        }))
    }

    /// Get current matrix profile
    pub async fn get_matrix_profile(&self) -> Result<Option<MatrixProfile>> {
        let xml = Self::build_request("MatrixGetProfile", &[]);
        let response = self.send_command(&xml).await?;

        // HQPlayer returns current profile - try both 'value' (as per Node.js reference)
        // and 'name' attribute for compatibility
        let index = Self::parse_attr_u32(&response, "index");
        let name =
            Self::parse_attr(&response, "value").or_else(|| Self::parse_attr(&response, "name"));

        match name {
            Some(n) if !n.is_empty() => Ok(Some(MatrixProfile { index, name: n })),
            _ => Ok(None),
        }
    }

    /// Set matrix profile by index - converts index to name for HQPlayer
    /// HQPlayer's MatrixSetProfile expects profile NAME, not index
    pub async fn set_matrix_profile(&self, profile_index: u32) -> Result<()> {
        // Get the list of profiles and find the name for this index
        let profiles = self.get_matrix_profiles().await?;
        let profile = profiles
            .iter()
            .find(|p| p.index == profile_index)
            .ok_or_else(|| anyhow::anyhow!("Matrix profile index {} not found", profile_index))?;

        let xml = Self::build_request("MatrixSetProfile", &[("value", &profile.name)]);
        self.send_command(&xml).await?;
        Ok(())
    }

    /// Get name of this instance (if set)
    pub async fn get_instance_name(&self) -> Option<String> {
        let state = self.state.read().await;
        state.instance_name.clone()
    }

    /// Set name of this instance
    pub async fn set_instance_name(&self, name: String) {
        let mut state = self.state.write().await;
        state.instance_name = Some(name);
    }

    /// Convert HQPlayer status to a unified bus Zone
    fn hqp_status_to_zone(
        host: &str,
        instance_name: Option<&str>,
        info: &HqpInfo,
        status: &HqpStatus,
        vol_range: &VolumeRange,
    ) -> BusZone {
        use std::time::{SystemTime, UNIX_EPOCH};

        let zone_id = format!("hqplayer:{}", instance_name.unwrap_or(host));
        let zone_name = if info.name.is_empty() {
            format!("HQPlayer @ {}", host)
        } else {
            info.name.clone()
        };

        let state = match status.state {
            0 => PlaybackState::Stopped,
            1 => PlaybackState::Paused,
            2 => PlaybackState::Playing,
            _ => PlaybackState::Unknown,
        };

        let volume_control = if vol_range.enabled {
            Some(BusVolumeControl {
                // Exact dB, not the rounded payload projection.
                value: status.volume_db as f32,
                min: vol_range.min_db as f32,
                max: vol_range.max_db as f32,
                step: vol_range.step_db.unwrap_or(f64::from(vol_range.step)) as f32,
                is_muted: false, // HQPlayer doesn't report mute separately
                scale: VolumeScale::Decibel,
                output_id: Some(zone_id.clone()),
            })
        } else {
            None
        };

        // Build now_playing if we have track info
        let now_playing = if !status.track_id.is_empty() || status.length > 0 {
            Some(BusNowPlaying {
                title: String::new(), // HQPlayer status doesn't include title
                artist: String::new(),
                album: String::new(),
                image_key: None,
                seek_position: Some(status.position as f64),
                duration: Some(status.length as f64),
                metadata: Some(TrackMetadata {
                    format: Some(status.active_mode.clone()),
                    sample_rate: Some(status.samplerate),
                    bit_depth: Some(status.active_bits as u8),
                    bitrate: Some(status.bitrate),
                    genre: None,
                    composer: None,
                    track_number: Some(status.track),
                    disc_number: None,
                }),
            })
        } else {
            None
        };

        let last_updated = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        BusZone {
            zone_id,
            zone_name,
            state,
            volume_control,
            now_playing,
            source: "hqplayer".to_string(),
            is_controllable: true,
            is_seekable: true,
            last_updated,
            is_play_allowed: state != PlaybackState::Playing,
            is_pause_allowed: state == PlaybackState::Playing,
            is_next_allowed: true,
            is_previous_allowed: true,
        }
    }
}

// =============================================================================
// Multi-instance manager
// =============================================================================

/// Instance info for API responses
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HqpInstanceInfo {
    pub name: String,
    pub host: Option<String>,
    pub port: u16,
    pub connected: bool,
    pub info: Option<HqpInfo>,
}

/// Manager for multiple HQPlayer instances
pub struct HqpInstanceManager {
    instances: Arc<RwLock<HashMap<String, Arc<HqpAdapter>>>>,
    bus: SharedBus,
}

impl HqpInstanceManager {
    /// Create a new instance manager
    pub fn new(bus: SharedBus) -> Self {
        Self {
            instances: Arc::new(RwLock::new(HashMap::new())),
            bus,
        }
    }

    /// Load instances from config file
    pub async fn load_from_config(&self) {
        let configs = load_hqp_configs();
        for config in configs {
            let adapter = Arc::new(HqpAdapter::new(self.bus.clone()));
            adapter.set_instance_name(config.name.clone()).await;
            adapter
                .configure(
                    config.host,
                    Some(config.port),
                    Some(config.web_port),
                    config.username,
                    config.password,
                )
                .await;

            let mut instances = self.instances.write().await;
            instances.insert(config.name, adapter);
        }
    }

    /// Save all instances to config file
    pub async fn save_to_config(&self) {
        // Clone adapters while holding lock, then release before async operations
        let adapters: Vec<(String, Arc<HqpAdapter>)> = {
            let instances = self.instances.read().await;
            instances
                .iter()
                .map(|(name, adapter)| (name.clone(), adapter.clone()))
                .collect()
        };

        let mut configs = Vec::new();
        for (name, adapter) in adapters {
            let status = adapter.get_status().await;
            if let Some(host) = status.host {
                let state = adapter.state.read().await;
                configs.push(HqpInstanceConfig {
                    name,
                    host,
                    port: status.port,
                    web_port: state.web_port,
                    username: state.web_username.clone(),
                    password: state.web_password.clone(),
                });
            }
        }

        save_hqp_configs(&configs);
    }

    /// Get or create an instance by name
    pub async fn get_or_create(&self, name: &str) -> Arc<HqpAdapter> {
        {
            let instances = self.instances.read().await;
            if let Some(adapter) = instances.get(name) {
                return adapter.clone();
            }
        }

        // Create new instance
        let adapter = Arc::new(HqpAdapter::new(self.bus.clone()));
        adapter.set_instance_name(name.to_string()).await;

        let mut instances = self.instances.write().await;
        instances.insert(name.to_string(), adapter.clone());
        adapter
    }

    /// Get an instance by name (if it exists)
    pub async fn get(&self, name: &str) -> Option<Arc<HqpAdapter>> {
        let instances = self.instances.read().await;
        instances.get(name).cloned()
    }

    /// Get the default instance (creates if not exists)
    pub async fn get_default(&self) -> Arc<HqpAdapter> {
        self.get_or_create("default").await
    }

    /// List all configured instances
    pub async fn list_instances(&self) -> Vec<HqpInstanceInfo> {
        // Clone adapters while holding lock, then release before async operations
        let adapters: Vec<(String, Arc<HqpAdapter>)> = {
            let instances = self.instances.read().await;
            instances
                .iter()
                .map(|(name, adapter)| (name.clone(), adapter.clone()))
                .collect()
        };

        let mut result = Vec::new();
        for (name, adapter) in adapters {
            let status = adapter.get_status().await;
            result.push(HqpInstanceInfo {
                name,
                host: status.host,
                port: status.port,
                connected: status.connected,
                info: status.info,
            });
        }

        result.sort_by(|a, b| a.name.cmp(&b.name));
        result
    }

    /// Add or update an instance
    pub async fn add_instance(
        &self,
        name: String,
        host: String,
        port: Option<u16>,
        web_port: Option<u16>,
        username: Option<String>,
        password: Option<String>,
    ) -> Arc<HqpAdapter> {
        let adapter = self.get_or_create(&name).await;
        adapter
            .configure(host, port, web_port, username, password)
            .await;
        self.save_to_config().await;
        adapter
    }

    /// Remove an instance by name
    pub async fn remove_instance(&self, name: &str) -> bool {
        let mut instances = self.instances.write().await;
        let removed = instances.remove(name).is_some();
        if removed {
            drop(instances);
            self.save_to_config().await;
        }
        removed
    }

    /// Check if any instance is configured
    pub async fn has_instances(&self) -> bool {
        let instances = self.instances.read().await;
        !instances.is_empty()
    }

    /// Get instance count
    pub async fn instance_count(&self) -> usize {
        let instances = self.instances.read().await;
        instances.len()
    }
}

// =============================================================================
// Zone linking service
// =============================================================================

const ZONE_LINKS_FILE: &str = "hqp-zone-links.json";

fn zone_links_path() -> PathBuf {
    get_config_file_path(ZONE_LINKS_FILE)
}

/// Zone link info for API responses
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZoneLink {
    pub zone_id: String,
    pub instance: String,
}

/// Service for managing zone-to-HQPlayer-instance links
pub struct HqpZoneLinkService {
    links: Arc<RwLock<HashMap<String, String>>>, // zone_id -> instance_name
    instances: Arc<HqpInstanceManager>,
}

impl HqpZoneLinkService {
    /// Create a new zone link service
    pub fn new(instances: Arc<HqpInstanceManager>) -> Self {
        let service = Self {
            links: Arc::new(RwLock::new(HashMap::new())),
            instances,
        };
        service.load_links_sync();
        service
    }

    /// Load links from disk synchronously (at startup)
    /// Issue #76: Uses read_config_file for backwards-compatible fallback
    fn load_links_sync(&self) {
        // read_config_file checks subdir first, falls back to root for legacy files
        if let Some(content) = read_config_file(ZONE_LINKS_FILE) {
            match serde_json::from_str::<HashMap<String, String>>(&content) {
                Ok(saved_links) => {
                    if let Ok(mut links) = self.links.try_write() {
                        *links = saved_links;
                        tracing::info!("Loaded {} HQP zone links from disk", links.len());
                    }
                }
                Err(e) => tracing::warn!("Failed to parse zone links: {}", e),
            }
        }
    }

    /// Save links to disk
    async fn save_links(&self) {
        let links = self.links.read().await;
        let path = zone_links_path();

        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        match serde_json::to_string_pretty(&*links) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&path, json) {
                    tracing::error!("Failed to save zone links: {}", e);
                } else {
                    tracing::debug!("Saved {} zone links to disk", links.len());
                }
            }
            Err(e) => tracing::error!("Failed to serialize zone links: {}", e),
        }
    }

    /// Link a zone to an HQP instance
    pub async fn link_zone(&self, zone_id: String, instance_name: String) -> Result<()> {
        // Verify instance exists
        if self.instances.get(&instance_name).await.is_none() {
            return Err(anyhow!("Unknown HQP instance: {}", instance_name));
        }

        {
            let mut links = self.links.write().await;
            links.insert(zone_id.clone(), instance_name.clone());
        }

        self.save_links().await;
        tracing::info!("Zone {} linked to HQP instance {}", zone_id, instance_name);
        Ok(())
    }

    /// Unlink a zone from HQP
    pub async fn unlink_zone(&self, zone_id: &str) -> bool {
        let was_linked = {
            let mut links = self.links.write().await;
            links.remove(zone_id).is_some()
        };

        if was_linked {
            self.save_links().await;
            tracing::info!("Zone {} unlinked from HQP", zone_id);
        }

        was_linked
    }

    /// Get the HQP instance name for a zone
    pub async fn get_instance_for_zone(&self, zone_id: &str) -> Option<String> {
        let links = self.links.read().await;
        links.get(zone_id).cloned()
    }

    /// Get all zone links
    pub async fn get_links(&self) -> Vec<ZoneLink> {
        let links = self.links.read().await;
        links
            .iter()
            .map(|(zone_id, instance)| ZoneLink {
                zone_id: zone_id.clone(),
                instance: instance.clone(),
            })
            .collect()
    }

    /// Get HQP pipeline data for a linked zone
    pub async fn get_pipeline_for_zone(&self, zone_id: &str) -> Option<PipelineStatus> {
        let instance_name = self.get_instance_for_zone(zone_id).await?;

        let adapter = self.instances.get(&instance_name).await?;
        if !adapter.is_configured().await {
            return None;
        }

        match adapter.get_pipeline_status().await {
            Ok(pipeline) => Some(pipeline),
            Err(e) => {
                tracing::error!("Failed to fetch HQP pipeline for zone {}: {}", zone_id, e);
                None
            }
        }
    }

    /// Remove all links pointing to a specific instance
    pub async fn remove_links_for_instance(&self, instance_name: &str) -> usize {
        let mut links = self.links.write().await;
        let zones_to_remove: Vec<String> = links
            .iter()
            .filter(|(_, inst)| *inst == instance_name)
            .map(|(zone_id, _)| zone_id.clone())
            .collect();

        let count = zones_to_remove.len();
        for zone_id in zones_to_remove {
            links.remove(&zone_id);
        }

        drop(links);

        if count > 0 {
            self.save_links().await;
            tracing::info!(
                "Removed {} zone links for deleted instance {}",
                count,
                instance_name
            );
        }

        count
    }

    /// Auto-correct links when instances are renamed (called after loading)
    pub async fn auto_correct_links(&self) -> bool {
        let instances = self.instances.list_instances().await;
        if instances.len() != 1 {
            return false; // Can only auto-correct with single instance
        }

        let single_instance = &instances[0].name;
        let mut corrected = false;

        {
            let mut links = self.links.write().await;
            let instance_names: Vec<String> = instances.iter().map(|i| i.name.clone()).collect();

            for (zone_id, instance_name) in links.iter_mut() {
                if !instance_names.contains(instance_name) {
                    tracing::warn!(
                        "Auto-correcting zone link {} from {} to {}",
                        zone_id,
                        instance_name,
                        single_instance
                    );
                    *instance_name = single_instance.clone();
                    corrected = true;
                }
            }
        }

        if corrected {
            self.save_links().await;
        }

        corrected
    }
}

// =============================================================================
// HQPlayer UDP multicast discovery
// =============================================================================

const HQP_MULTICAST_ADDR: Ipv4Addr = Ipv4Addr::new(239, 192, 0, 199);
const HQP_DISCOVERY_PORT: u16 = 4321;
const HQP_DISCOVERY_TIMEOUT_MS: u64 = 3000;

/// Discovered HQPlayer instance
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredHqp {
    pub host: String,
    pub port: u16,
    pub name: String,
    pub version: String,
    pub product: Option<String>,
}

/// Discover HQPlayer instances on the network via UDP multicast
pub async fn discover_hqplayers(timeout_ms: Option<u64>) -> Result<Vec<DiscoveredHqp>> {
    let timeout_duration = Duration::from_millis(timeout_ms.unwrap_or(HQP_DISCOVERY_TIMEOUT_MS));
    let mut discovered: HashMap<String, DiscoveredHqp> = HashMap::new();

    // Create UDP socket
    let socket = UdpSocket::bind("0.0.0.0:0").await?;

    // Join multicast group
    socket.set_broadcast(true)?;

    // Send discovery message
    let message = b"<?xml version=\"1.0\"?><discover>hqplayer</discover>";
    let dest = SocketAddrV4::new(HQP_MULTICAST_ADDR, HQP_DISCOVERY_PORT);
    socket.send_to(message, dest).await?;

    tracing::debug!(
        "Sent HQPlayer discovery multicast to {}:{}",
        HQP_MULTICAST_ADDR,
        HQP_DISCOVERY_PORT
    );

    // Receive responses with timeout
    let mut buf = [0u8; 2048];
    let deadline = tokio::time::Instant::now() + timeout_duration;

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }

        match timeout(remaining, socket.recv_from(&mut buf)).await {
            Ok(Ok((len, addr))) => {
                let response = String::from_utf8_lossy(&buf[..len]);
                tracing::debug!("HQP discovery response from {}: {}", addr, response);

                // Parse XML response
                if let Some(hqp) = parse_discovery_response(&response, addr.ip().to_string()) {
                    discovered.insert(hqp.host.clone(), hqp);
                }
            }
            Ok(Err(e)) => {
                tracing::warn!("HQP discovery recv error: {}", e);
                break;
            }
            Err(_) => {
                // Timeout - done receiving
                break;
            }
        }
    }

    let result: Vec<DiscoveredHqp> = discovered.into_values().collect();
    tracing::info!("HQPlayer discovery found {} instance(s)", result.len());
    Ok(result)
}

/// Parse HQPlayer discovery XML response
fn parse_discovery_response(xml: &str, host: String) -> Option<DiscoveredHqp> {
    // Look for <discover result="OK" .../>
    if !xml.contains("result=\"OK\"") && !xml.contains("result='OK'") {
        return None;
    }

    let name = extract_xml_attr(xml, "name").unwrap_or_else(|| "HQPlayer".to_string());
    let version = extract_xml_attr(xml, "version").unwrap_or_else(|| "unknown".to_string());
    let product = extract_xml_attr(xml, "product");

    Some(DiscoveredHqp {
        host,
        port: HQP_DISCOVERY_PORT,
        name,
        version,
        product,
    })
}

/// Extract attribute value from XML string
fn extract_xml_attr(xml: &str, attr: &str) -> Option<String> {
    // Try double quotes
    let pattern = format!("{}=\"", attr);
    if let Some(start) = xml.find(&pattern) {
        let value_start = start + pattern.len();
        if let Some(end) = xml[value_start..].find('"') {
            return Some(xml[value_start..value_start + end].to_string());
        }
    }

    // Try single quotes
    let pattern = format!("{}='", attr);
    if let Some(start) = xml.find(&pattern) {
        let value_start = start + pattern.len();
        if let Some(end) = xml[value_start..].find('\'') {
            return Some(xml[value_start..value_start + end].to_string());
        }
    }

    None
}
