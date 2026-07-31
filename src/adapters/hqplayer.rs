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

mod lifecycle;
pub use lifecycle::{HqpRecoveryConfig, HqpWorkerPhase, HqpWorkerStatus};

use anyhow::{anyhow, Context, Result};
use quick_xml::events::{BytesStart, Event};
use quick_xml::Writer;
use regex::Regex;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Cursor;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::{Mutex, RwLock};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::adapters::Startable;
use crate::bus::{
    BusEvent, NowPlaying as BusNowPlaying, PlaybackState, PrefixedZoneId, SharedBus, TrackMetadata,
    VolumeControl as BusVolumeControl, VolumeScale, Zone as BusZone,
};
use crate::config::{get_config_file_path, read_config_file};
use lifecycle::{supervise_worker, RecoveryState, SharedRecoveryState, SharedWorkerPhase};

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
            Scan::Complete { .. } => Framing::Complete,
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
        first_document_span(buf).map(|(_, end)| end)
    }

    /// Byte range of the **first** complete document in `buf`, as `(start, end)`.
    ///
    /// `start` is where the document's own prologue or root begins, so any leading bytes belonging to
    /// nothing — the tail of a fragment, say — are outside the span. Returning the span rather than
    /// only the end is what lets a caller hand back *exactly* the document it selected instead of the
    /// document plus whatever preceded it: attribute lookups scope to a root open tag, and a scope
    /// that cannot start is a scope that falls back to searching everything.
    pub fn first_document_span(buf: &str) -> Option<(usize, usize)> {
        match scan(buf) {
            Scan::Complete { start, end } => Some((start, end)),
            _ => None,
        }
    }

    /// Outcome of one framing walk. [`classify`] and [`first_document_end`] are both projections of
    /// it, so there is one traversal to keep correct rather than two that can disagree.
    enum Scan {
        /// A document occupied this byte range. `start` excludes anything that preceded its prologue.
        Complete {
            start: usize,
            end: usize,
        },
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
        // Where this document begins. Set at the first structurally significant event — a
        // declaration, or the root's own tag — so leading text that belongs to no document is left
        // outside the span.
        let mut start: Option<usize> = None;

        loop {
            let before = reader.buffer_position() as usize;
            match reader.read_event() {
                Ok(Event::Start(e)) => {
                    start.get_or_insert(before);
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
                            return Scan::Complete {
                                start: start.unwrap_or(0),
                                end: reader.buffer_position() as usize,
                            }
                        }
                        Some(_) => {}
                    }
                }
                Ok(Event::Empty(_)) => {
                    let at = *start.get_or_insert(before);
                    if open.is_empty() {
                        return Scan::Complete {
                            start: at,
                            end: reader.buffer_position() as usize,
                        };
                    }
                }
                // Ran out of bytes, or hit a token quick_xml cannot read. Normally that means more is
                // coming — but if the root's own closing tag is already present, the frame *is* closed
                // and only the children are unreadable. See `root_frame_end`.
                Ok(Event::Eof) | Err(_) => {
                    return match root_frame_end(buf) {
                        Some(end) => Scan::Complete {
                            start: start.unwrap_or(0),
                            end,
                        },
                        None => Scan::Incomplete,
                    }
                }
                // A declaration opens a document even though it carries no framing weight itself.
                Ok(Event::Decl(_)) => {
                    start.get_or_insert(before);
                }
                // Text, comments and processing instructions carry no framing weight.
                Ok(_) => {}
            }
        }
    }

    /// Byte offset just past the **first defensible** occurrence of the root element's own closing
    /// tag, or `None` if there is none.
    ///
    /// This is the recovery boundary for a document whose *children* cannot be parsed. The daemon
    /// emits malformed XML inside `<metadata>`, and a child tag that never terminates makes a parser
    /// consume the `</Status>` that follows it as part of the child's own attribute soup — so the
    /// closing tag is in the buffer but was never seen as an end event. Without recovery such a reply
    /// reads as incomplete and the command burns its whole deadline waiting for bytes it already
    /// holds, on **every** poll while a track is loaded.
    ///
    /// Reaching here at all is the diagnosis: had that tag been parsed as an end event, the element
    /// stack would have emptied and `Complete` would already have been returned.
    ///
    /// "First defensible" means the first occurrence that is markup rather than content. Each clause
    /// below protects a framing guarantee #322 exists to hold:
    ///
    /// * It keys on the root's **own** name, so `<State …></Status>` is not rescued — mismatched
    ///   nesting is decided by the name-comparison path in [`scan`], which is reached first.
    /// * It skips every region where XML says `<` is not markup: quoted attribute values in **either**
    ///   quote form, comments, CDATA sections, and processing instructions (plus any other `<!`
    ///   declaration, conservatively). That set is closed, not open-ended. So a `</Status>` sitting in
    ///   an attribute value or a comment is data and never a boundary.
    /// * A document truncated before its real closing tag therefore has no boundary to find and stays
    ///   incomplete — it is never credited with a tag that has not arrived.
    /// * It is consulted only when the parse could not complete on its own, so every well-formed
    ///   document takes the unchanged path.
    ///
    /// The one shape it deliberately declines to resolve is an **unterminated** attribute quote: with
    /// the quote still open there is no way to tell markup from data, so it finds no boundary rather
    /// than guessing at one.
    ///
    /// Taking the *first* such occurrence rather than requiring the closing tag to be the buffer's last
    /// token is load-bearing, because the daemon both emits malformed `<metadata>` children *and*
    /// pushes `Status` frames unprompted: a hostile reply with a push frame coalesced behind it has a
    /// closing tag that is not last, and a last-token rule left it unrecovered for a whole deadline.
    ///
    /// Two implementation notes. The root's name comes from [`root_element`] rather than a private
    /// scan, so this shares one tokeniser with the rest of the module. And a self-closing root cannot
    /// reach here at all — quick_xml reports it as `Event::Empty`, which [`scan`] answers before any
    /// child is read.
    ///
    /// Attribute reads are unaffected either way: [`root_open_tag`] is a quote-aware scan that stops at
    /// the root tag's own `>`, so it never looks at a child.
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
    ///
    /// Quote tracking records **which** character opened the current attribute value, not merely that
    /// one is open, for the same reason [`root_frame_end`] does: XML permits both forms and only the
    /// matching character closes a value. Tracking `"` alone stopped the scan at the `>` inside
    /// `song='a>b'`, and because every attribute after that point then read as absent — with callers
    /// substituting `0`/`""` rather than reporting a parse failure — a track title containing `>`
    /// silently zeroed the volume the UI showed. An unterminated quote yields no scope at all rather
    /// than a guessed one, again matching [`root_frame_end`].
    pub fn root_open_tag(buf: &str) -> Option<&str> {
        let rest = skip_prologue(buf)?;
        if !rest.starts_with('<') || rest.starts_with("</") {
            return None;
        }
        let bytes = rest.as_bytes();
        let mut quote: Option<u8> = None;
        for (i, b) in bytes.iter().enumerate() {
            match b {
                b'"' | b'\'' => match quote {
                    // Only the character that opened the value can close it.
                    Some(open) if open == *b => quote = None,
                    Some(_) => {}
                    None => quote = Some(*b),
                },
                b'>' if quote.is_none() => return Some(&rest[..=i]),
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
/// Checked **before** each append, against `already_held + chunk_len`, so the accumulation never
/// exceeds this ceiling (see the read loop in `send_command_inner`). An earlier line-based reader
/// checked *after* appending a whole line, which put the true peak one line above the ceiling; that
/// is no longer how framing reads — it accumulates fixed-size [`RESPONSE_READ_CHUNK`] chunks and
/// rejects the connection before a chunk that would breach the ceiling is ever appended.
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

/// What a live setting write actually did.
///
/// `result="OK"` is not proof of application (HQP-C-028), and equality with pre-existing state is not
/// proof either (HQP-C-019): requesting Auto under `[source]`, where the rate is already Auto and the
/// daemon ignores the pin, compares 0 against 0 and looks like success. So each distinct thing a write
/// can turn out to be gets its own name rather than being collapsed into `Ok`/`Err`, and the caller
/// decides which of them counts as success on its surface. There are five, below, plus a sixth case
/// that is deliberately not one of them:
///
/// An explicit `result="Error"` stays an `Err` — [`HqpAdapter::check_result`] raises it inside
/// `send_command` as a typed [`HqpRejected`], so it carries the daemon's own reason and is
/// distinguishable from every variant here by being an `Err` at all. It is not a variant because a
/// rejection is not an outcome of a *setting*: nothing was set, nothing is unknown, and there is
/// nothing to read back.
///
/// # What this does not cover, and why
///
/// **Volume is outside this vocabulary and stays `Result<()>`.** `Volume`, `VolumeUp`, `VolumeDown`
/// and `VolumeMute` are result-checked but never readback-verified, which is a #322 decision this
/// issue keeps rather than an omission: a fixed-volume daemon answers an explicit `result="Error"`
/// that `check_result` already surfaces, and with **adaptive volume engaged the daemon moves the
/// level on its own** (`VolumeRange.adaptive`), so a readback comparison would manufacture failures
/// for writes that landed exactly as asked. Volume is also absolute dB rather than a list index
/// (HQP-C-040), so it is not in the enumerated-setting domain these outcomes describe at all. Claiming
/// "every setter reports an outcome" would therefore be an overclaim; the enumerated live settings —
/// mode, filter 1x/Nx, shaper, rate, matrix profile — are what this covers.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use = "an unexamined outcome is a setting reported as applied when it may not have been"]
pub enum SettingOutcome {
    /// Sent, acknowledged, and the authoritative `State` field now reads the requested value.
    Applied,
    /// **Nothing was sent.** The authoritative `State` field already read the requested value.
    ///
    /// Distinct from [`Self::Applied`] because for `SetMode` it is not an optimisation but a
    /// correctness requirement: a same-mode `SetMode` still clears the exact-rate pin (HQP-C-017), so
    /// writing a mode the daemon is already in destroys user state for nothing.
    AlreadySet,
    /// Sent and acknowledged, and the authoritative field never moved.
    Ignored {
        what: String,
        requested: String,
        observed: String,
    },
    /// **Never sent, and nothing mutated.** The client refused, with a stated reason.
    Suppressed { what: String, reason: String },
    /// The write was attempted and its outcome cannot be established — a lost reply is ambiguous
    /// delivery, never proof of non-application (HQP-C-029).
    Ambiguous { what: String, reason: String },
}

impl SettingOutcome {
    /// Whether the authoritative state now carries the requested value.
    pub fn is_applied(&self) -> bool {
        matches!(self, Self::Applied | Self::AlreadySet)
    }

    /// Collapse to the `Result<()>` the HTTP and MCP surfaces already answer with, so no response
    /// contract changes: anything that is not applied becomes an error carrying the stated reason.
    pub fn into_applied_result(self) -> Result<()> {
        match self {
            Self::Applied | Self::AlreadySet => Ok(()),
            Self::Ignored {
                what,
                requested,
                observed,
            } => Err(anyhow!(
                "HQPlayer accepted {what} but {what} still reads {observed} instead of {requested}; \
                 refusing to report an unverified change"
            )),
            Self::Suppressed { what, reason } => {
                Err(anyhow!("HQPlayer {what} was not changed: {reason}"))
            }
            Self::Ambiguous { what, reason } => Err(anyhow!(
                "HQPlayer {what} may or may not have changed: {reason}"
            )),
        }
    }
}

/// The daemon answered `result="Error"`: an **explicit rejection**, carrying its own reason.
///
/// A distinct type rather than a message, because the difference is load-bearing. A rejection is
/// terminal — the daemon saw the request, refused it, and nothing changed — while a lost reply is
/// *ambiguous delivery*: the daemon may have accepted, logged and acted on the request and then sent
/// nothing (HQP-C-029). One must not be retried or read back; the other must. Telling them apart by
/// matching on error text is how that distinction quietly stops holding.
///
/// `Display` is unchanged from the message this replaced, so a caller reading the string sees exactly
/// what it saw before.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HqpRejected {
    /// The request element the daemon echoed back.
    pub element: String,
    /// The reason it carried as element text, when it carried one.
    pub reason: Option<String>,
}

impl std::fmt::Display for HqpRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.reason {
            Some(reason) => write!(f, "HQPlayer rejected {}: {}", self.element, reason),
            None => write!(f, "HQPlayer rejected {} (no reason given)", self.element),
        }
    }
}

impl std::error::Error for HqpRejected {}

/// A semantic operation was about to continue on a different native transport.
///
/// This is deliberately typed: a caller can restart safely before a write, while a caller that has
/// already attempted a write must report ambiguity. Matching error text would erase that boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HqpSessionChanged {
    expected: u64,
    actual: u64,
}

impl std::fmt::Display for HqpSessionChanged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.expected == self.actual {
            write!(
                f,
                "HQPlayer native session generation {} is no longer installed",
                self.expected
            )
        } else {
            write!(
                f,
                "HQPlayer native session changed from generation {} to {}",
                self.expected, self.actual
            )
        }
    }
}

impl std::error::Error for HqpSessionChanged {}

/// The semantic family of a mode entry, so a caller never depends on a list position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModeFamily {
    /// Follow the source: the daemon's `[source]`.
    Source,
    Pcm,
    /// Sigma-delta modulation, which the daemon names `"SDM (DSD)"` and callers ask for as `"DSD"`.
    Sdm,
}

/// A setting family whose **legacy** HTTP request contract carries a list position rather than a name.
///
/// Named so the compatibility boundary is a visible, single place rather than a habit. See
/// [`HqpAdapter::legacy_index_to_name`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacySettingFamily {
    Mode,
    Filter,
    Shaper,
}

/// The semantic operation selected by a legacy numeric request.
///
/// Unlike [`LegacySettingFamily`], this retains which filter side the frozen HTTP contract named so
/// index resolution and the resulting write can execute under one endpoint operation lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacySettingTarget {
    Mode,
    FilterPair,
    Filter1x,
    FilterNx,
    Shaper,
}

impl LegacySettingTarget {
    fn family(self) -> LegacySettingFamily {
        match self {
            Self::Mode => LegacySettingFamily::Mode,
            Self::FilterPair | Self::Filter1x | Self::FilterNx => LegacySettingFamily::Filter,
            Self::Shaper => LegacySettingFamily::Shaper,
        }
    }
}

/// Which half of the filter pair a one-sided request is aimed at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FilterSide {
    OneX,
    Nx,
}

impl FilterSide {
    /// The `State` field that is authoritative for this side, named as the caller sees it.
    fn field(self) -> &'static str {
        match self {
            Self::OneX => "filter1x",
            Self::Nx => "filterNx",
        }
    }
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
    /// Track title from the optional `Status/metadata` child.
    ///
    /// Kept internal until the public HQPlayer payload contract is explicitly revised; the unified
    /// zone projection can still publish it through the existing now-playing shape.
    #[serde(skip)]
    pub title: Option<String>,
    /// Track artist from the optional `Status/metadata` child.
    #[serde(skip)]
    pub artist: Option<String>,
    /// Track album from the optional `Status/metadata` child.
    #[serde(skip)]
    pub album: Option<String>,
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
///
/// `PartialEq` because a freshly fetched family is compared against the cached one *whole*, not
/// only through its fingerprint. The fingerprint covers `(index, name)` — the pair a list index is
/// resolved through — so two lists agreeing on it can still differ in the fields it does not cover,
/// and a publisher that skipped the write on a fingerprint match alone would drop those silently.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListItem {
    pub index: u32,
    pub name: String,
    pub value: i32, // Can be negative (e.g., -1 for PCM mode)
}

/// Rate item
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateItem {
    pub index: u32,
    pub rate: u32,
}

/// Filter item with arg
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

/// One HQPlayer native observation whose state, runtime enumerations, transport identity and
/// volume capability have passed the same generation fence. This is intentionally private: the
/// legacy HTTP pipeline payload remains [`PipelineStatus`], while the managed adaptive producer
/// needs facts that payload historically rounds or omits.
#[derive(Debug, Clone)]
struct CoherentPipelineSnapshot {
    legacy: PipelineStatus,
    state: HqpState,
    playback_status: HqpStatus,
    chain: ChainSnapshot,
    volume_range: VolumeRange,
    info: HqpInfo,
    host: String,
    instance_name: String,
    producer_epoch: u64,
    observed_at: SystemTime,
}

#[derive(Debug, Clone)]
struct CoherentSessionIdentity {
    info: HqpInfo,
    host: String,
    instance_name: String,
    producer_epoch: u64,
}

/// Contract-free observation exported by the native adapter. Publication policy and adaptive
/// schema projection belong to the composition layer in `src/producers`.
#[derive(Debug, Clone, PartialEq)]
pub struct HqpNativeObservation {
    pub instance_name: String,
    pub instance_label: Option<String>,
    pub product_version: Option<String>,
    pub producer_epoch: u64,
    pub observed_at: SystemTime,
    pub transport: HqpNativeTransportState,
    pub metadata: HqpNativeMetadata,
    pub volume: HqpNativeVolume,
    pub mode_is_source: bool,
    pub mode: HqpNativeSelection,
    pub filter_1x: HqpNativeSelection,
    pub filter_nx: HqpNativeSelection,
    pub shaper: HqpNativeSelection,
    pub rate: HqpNativeSelection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HqpNativeTransportState {
    Stopped,
    Paused,
    Playing,
    Unknown,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HqpNativeMetadata {
    pub track_id: Option<String>,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HqpNativeVolume {
    pub value_db: f64,
    pub min_db: f64,
    pub max_db: f64,
    pub step_db: Option<f64>,
    pub enabled: bool,
    pub adaptive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HqpNativeSelection {
    pub selected: String,
    pub choices: Vec<String>,
}

/// Dependency-inversion seam for managed native observations. The adapter reports facts and
/// lifecycle only; the injected composition-root implementation owns schema projection,
/// revisions, publication leases, retention and retirement.
#[async_trait::async_trait]
pub trait HqpNativeObservationSink: Send + Sync {
    async fn manager_started(&self) -> Result<()>;
    async fn observed(&self, observation: HqpNativeObservation) -> Result<()>;
    async fn transient_failure(&self, instance_name: &str, observed_at: SystemTime) -> Result<()>;
    async fn instance_removed(&self, instance_name: &str, producer_epoch: u64) -> Result<()>;
    async fn manager_stopped(&self) -> Result<()>;
}

#[derive(Clone)]
struct HqpNativeWorker {
    sink: Arc<dyn HqpNativeObservationSink>,
    instance_name: String,
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
    /// Bytes read from the socket that belong to no completed reply yet — everything after the
    /// document `send_command_inner` returned.
    ///
    /// Without this, a read carrying a reply plus only the **prefix** of a coalesced unsolicited
    /// frame discarded that prefix while its suffix stayed in the socket, and the next command
    /// concatenated the orphan with its own reply. A shared attribute in the push then overrode the
    /// real one silently, because the reply's root still matched what was expected.
    ///
    /// Keeping the prefix means the follower completes on a later read and is recognised and skipped
    /// as the document it is, so the orphan never forms. Bounded by construction: this is always a
    /// suffix of an accumulation already capped at [`MAX_RESPONSE_BYTES`], and it lives on the
    /// connection, so a reconnect starts empty and cannot inherit another socket's bytes.
    carry: Vec<u8>,
}

/// Owns the native socket while one request/reply conversation is in flight.
///
/// Async cancellation skips ordinary error handling. If this guard is dropped while it still owns
/// the socket, it synchronously marks the transport poisoned before dropping the socket; the next
/// native operation performs the async state/bus cleanup before reconnecting.
struct InFlightConnection {
    connection: Option<HqpConnection>,
    poisoned: Arc<std::sync::atomic::AtomicBool>,
}

impl InFlightConnection {
    fn new(connection: HqpConnection, poisoned: Arc<std::sync::atomic::AtomicBool>) -> Self {
        Self {
            connection: Some(connection),
            poisoned,
        }
    }

    fn connection_mut(&mut self) -> Result<&mut HqpConnection> {
        self.connection
            .as_mut()
            .ok_or_else(|| anyhow!("in-flight HQPlayer connection was already consumed"))
    }

    fn restore(mut self, slot: &mut Option<HqpConnection>) {
        *slot = self.connection.take();
    }
}

impl Drop for InFlightConnection {
    fn drop(&mut self) {
        if self.connection.is_some() {
            self.poisoned
                .store(true, std::sync::atomic::Ordering::Release);
        }
    }
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

/// The identity of a complete set of cached chain-scoped enumerations.
///
/// Four fingerprints rather than one. The rate list alone is the right *probe* for a chain move —
/// smallest chain-scoped enumeration, and the two chains' rate lists were observed disjoint
/// (HQP-C-020) — and it is the wrong *identity*, because a profile load can replace filters and
/// shapers while leaving rates byte-identical. A read that captured only the rates would accept such
/// a replacement as the set it started from.
///
/// And a generation alongside them, because four fingerprints are still not enough. They describe
/// *what* is cached, and a clear-then-refill can arrive back at the same four — the same daemon
/// reloading the same configuration — while the event that emptied the cache has changed the
/// session, the profile or the loaded chain, and therefore changed `State`. Contents alone cannot
/// distinguish "untouched" from "torn down and rebuilt identically"; only a counter of fillings can,
/// and that distinction is what a read holding a pre-clear identity and a post-clear `State` needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ChainIdentity {
    modes: u64,
    filters: u64,
    shapers: u64,
    rates: u64,
    generation: u64,
}

/// The four chain-scoped families, taken together, as one value.
///
/// Returned by [`HqpAdapter::verified_chain_snapshot`] so that what a read *verified* and what it
/// *publishes* are the same values rather than two reads of the same cache, and accepted by
/// [`HqpAdapter::publish_chain_snapshot`] so that a gathered set is offered to the cache as one
/// thing rather than four assignments that could each be judged separately.
#[derive(Debug, Clone)]
struct ChainSnapshot {
    modes: Vec<ListItem>,
    filters: Vec<FilterItem>,
    shapers: Vec<ListItem>,
    rates: Vec<RateItem>,
}

/// Result of the optional, non-chain-scoped volume-capability read.
enum PipelineVolumeRange {
    Observed { range: VolumeRange, generation: u64 },
    FixedFallback(VolumeRange),
}

/// **One** freshly fetched chain-scoped family, on its way into the cache.
///
/// The counterpart of [`ChainSnapshot`] for the single-family path, and one value rather than a
/// `(family_name, list, fingerprint)` triple for a specific reason: those three had drifted apart.
/// The removed `note_enumeration` took a `&str` to decide *which* previous fingerprint to compare,
/// while its caller separately assigned the list and separately assigned a fingerprint it had
/// computed itself — three spellings of "which family is this" that nothing made agree, passed
/// across two different lock acquisitions. Here the family, the contents and the fingerprint are one
/// value, the fingerprint is derived from the contents rather than accepted alongside them (the
/// invariant [`HqpAdapter::chain_identity_of`] rests on), and the whole decision happens under one
/// lock in [`HqpAdapter::publish_fresh_family`].
#[derive(Debug, Clone)]
enum FreshFamily {
    Modes(Vec<ListItem>),
    Filters(Vec<FilterItem>),
    Shapers(Vec<ListItem>),
    Rates(Vec<RateItem>),
}

impl FreshFamily {
    /// The daemon's name for this family, for the line a transition logs.
    fn label(&self) -> &'static str {
        match self {
            Self::Modes(_) => "modes",
            Self::Filters(_) => "filters",
            Self::Shapers(_) => "shapers",
            Self::Rates(_) => "rates",
        }
    }

    /// The identity of these contents, derived from them and from nothing else.
    ///
    /// Delegates to the same per-family functions [`SemanticFamily`] carries, so a setter's proof and
    /// the cache cannot disagree about what makes two enumerations the same enumeration.
    fn fingerprint(&self) -> u64 {
        match self {
            Self::Modes(items) | Self::Shapers(items) => HqpAdapter::list_fingerprint(items),
            Self::Filters(items) => HqpAdapter::filters_fingerprint(items),
            Self::Rates(items) => HqpAdapter::rates_fingerprint(items),
        }
    }

    /// The fingerprint the cache currently holds for *this* family, or `None` if it holds none —
    /// which is what a family never fetched and a family just cleared both look like.
    fn cached_fingerprint(&self, state: &HqpAdapterState) -> Option<u64> {
        match self {
            Self::Modes(_) => state.modes_fingerprint,
            Self::Filters(_) => state.filters_fingerprint,
            Self::Shapers(_) => state.shapers_fingerprint,
            Self::Rates(_) => state.rates_fingerprint,
        }
    }

    /// Whether these contents are, element for element, what the cache already holds.
    ///
    /// Asked in addition to the fingerprint rather than instead of it. The fingerprint answers "does
    /// this list resolve indices the same way"; this answers "is writing it a no-op", and only the
    /// second licenses skipping the write.
    fn matches_cache(&self, state: &HqpAdapterState) -> bool {
        match self {
            Self::Modes(items) => *items == state.modes,
            Self::Filters(items) => *items == state.filters,
            Self::Shapers(items) => *items == state.shapers,
            Self::Rates(items) => *items == state.rates,
        }
    }

    /// Whether this reply resolves every position exactly as the cached winner does.
    ///
    /// Narrower than [`Self::matches_cache`]: a setter only needs the `(index, semantic value)`
    /// mapping to be current. If a complete refresh overtook this fetch but published that exact
    /// mapping, the reply is safe to resolve through even if non-routing metadata differs. The
    /// winner remains authoritative and is never rewritten by this check.
    fn resolves_like_cache(&self, state: &HqpAdapterState) -> bool {
        match self {
            Self::Modes(items) => items
                .iter()
                .map(|item| (item.index, item.name.as_str()))
                .eq(state
                    .modes
                    .iter()
                    .map(|item| (item.index, item.name.as_str()))),
            Self::Filters(items) => items
                .iter()
                .map(|item| (item.index, item.name.as_str()))
                .eq(state
                    .filters
                    .iter()
                    .map(|item| (item.index, item.name.as_str()))),
            Self::Shapers(items) => items
                .iter()
                .map(|item| (item.index, item.name.as_str()))
                .eq(state
                    .shapers
                    .iter()
                    .map(|item| (item.index, item.name.as_str()))),
            Self::Rates(items) => items
                .iter()
                .map(|item| (item.index, item.rate))
                .eq(state.rates.iter().map(|item| (item.index, item.rate))),
        }
    }

    /// Install these contents **and** the fingerprint derived from them, together, so that no path
    /// can leave a family anchored to an identity it was not filled from.
    fn install(self, state: &mut HqpAdapterState, fingerprint: u64) {
        match self {
            Self::Modes(items) => {
                state.modes = items;
                state.modes_fingerprint = Some(fingerprint);
            }
            Self::Filters(items) => {
                state.filters = items;
                state.filters_fingerprint = Some(fingerprint);
            }
            Self::Shapers(items) => {
                state.shapers = items;
                state.shapers_fingerprint = Some(fingerprint);
            }
            Self::Rates(items) => {
                state.rates = items;
                state.rates_fingerprint = Some(fingerprint);
            }
        }
    }
}

/// **What a semantic setter has to prove, gathered per setting family.**
///
/// A setter that accepts a *name* and speaks *positions* holds two domains together, and the join is
/// not durable. `resolve` reads a position out of the enumeration the daemon serves at that moment;
/// every comparison after it — the `AlreadySet` no-op check, the post-write readback — compares that
/// **number** against `State`. Under a configured `[source]` mode the loaded chain moves when the
/// source material does (HQP-C-007), so it can move between those two requests, and where two chains'
/// lists are the same length a stale position is *in range* and names a different setting
/// (HQP-C-009). The number then agrees with itself and the setter reports `AlreadySet` or `Applied`
/// for a value the daemon does not hold.
///
/// **The cache generation cannot see this one.** A source-driven transition has no competing cache
/// writer: nothing invalidates, nothing is overtaken, every guard in
/// [`HqpAdapter::publish_fresh_family`] passes. What detects it is re-reading the enumeration and
/// finding a different one.
///
/// The five pieces live in one value rather than arriving as five arguments because they have to
/// agree with each other: `resolve` and `describe` are the two directions of a single mapping,
/// `fingerprint` is the identity that mapping is only valid inside, and `selected` names the one
/// `State` field that is authoritative for the setting. Three spellings of "which family is this" is
/// the shape [`FreshFamily`] was introduced to remove.
struct SemanticFamily<T: 'static, R: ?Sized + 'static> {
    /// The setting, named as a caller sees it, for the outcome it reports.
    what: &'static str,
    /// Whether one semantic request addresses both 1x and Nx and therefore must name both in a
    /// negative report.
    paired: bool,
    /// Whether the final proof must reject configured source-following mode. Rate pins are accepted
    /// and ignored there, including the numerically indistinguishable Auto case.
    refuses_source_mode: bool,
    /// Identity of an enumeration: equal for two lists exactly when they resolve every position the
    /// same way.
    fingerprint: fn(&[T]) -> u64,
    /// The position the requested semantic value occupies in a given list.
    resolve: fn(&[T], &R) -> Result<u32>,
    /// What the daemon currently holds, read from the field that *is* this setting. A paired setting
    /// must preserve the difference between two reported positions and an absent position: the
    /// former disproves application while the latter proves nothing.
    selected: fn(&HqpState) -> SemanticSelection,
    /// The semantic value at a position, for the report a human reads.
    describe: fn(&[T], u32) -> String,
}

/// The authoritative state observed for one semantic setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SemanticSelection {
    /// The setting is reported as one list position.
    Position(u32),
    /// A paired setting reports two different positions.
    Split(u32, u32),
    /// The authoritative field (or one side of a pair) is absent.
    Missing,
}

/// Why a read could not join the `State` it observed to the lists it holds.
///
/// Named cases rather than a bare `bool`, because they do not mean the same thing: only `Moved` is
/// the daemon contradicting the cache, and only that one justifies dropping it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChainProofFailure {
    /// A family went missing mid-read — a reconnect, most likely.
    Partial,
    /// The daemon's live rate list is not the one the cached lists were read from.
    Moved,
    /// A complete, coherent, *different* set is in the cache: it was replaced mid-read.
    Replaced,
}

impl std::fmt::Display for ChainProofFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let reason = match self {
            Self::Partial => {
                "its setting lists were dropped while its state was being read, most likely by a \
                 reconnect"
            }
            Self::Moved => {
                "its loaded chain moved while its state was being read, so the rate list it serves \
                 now is not the one those lists came from"
            }
            Self::Replaced => {
                "its setting lists were replaced while its state was being read, so that state's \
                 indices belong to a set this read never verified"
            }
        };
        f.write_str(reason)
    }
}

/// Internal adapter state.
#[allow(dead_code)]
struct HqpAdapterState {
    instance_name: Option<String>,
    host: Option<String>,
    port: u16,
    web_port: u16,
    web_username: Option<String>,
    web_password: Option<String>,
    /// Monotonic identity of the configured native + persistent endpoint.
    ///
    /// Internal provenance only. The operation mutex prevents this changing inside a semantic
    /// operation; the generation makes that invariant inspectable without exposing a public field.
    endpoint_generation: u64,
    /// Monotonic identity of the installed native socket, including same-address replacement.
    transport_generation: u64,
    connected: bool,
    /// Monotonic identity of the last connection that completed a coherent handshake.
    ///
    /// A fast reconnect may pass through `connected=false` between two consumer observations, so
    /// correctness cannot depend on sampling that transient edge. This advances only when the new
    /// socket has produced the complete initial direct-zone snapshot.
    producer_epoch: u64,
    info: Option<HqpInfo>,
    last_state: Option<HqpState>,
    modes: Vec<ListItem>,
    filters: Vec<FilterItem>,
    shapers: Vec<ListItem>,
    rates: Vec<RateItem>,
    /// Fingerprints of the enumerations these caches were filled from.
    ///
    /// **This is how the loaded chain is identified.** The chain is not derived from the configured
    /// mode — under `[source]` those are different questions (HQP-C-007) — nor from `State.active_mode`,
    /// whose semantics under `[source]` are unmeasured (HQP-C-024) and which UHC therefore must not
    /// read as an answer. It is derived from the only authority that is settled: the enumerations are
    /// chain-scoped and change wholesale when the chain does (HQP-C-008, `E0-uhc-live`). So a freshly
    /// fetched list whose fingerprint differs from the cached one **is** a chain transition, and every
    /// other chain-scoped family is stale from that moment.
    ///
    /// `None` means "never observed", which is not a transition.
    modes_fingerprint: Option<u64>,
    filters_fingerprint: Option<u64>,
    shapers_fingerprint: Option<u64>,
    rates_fingerprint: Option<u64>,
    /// How many times the chain-scoped cache has been cleared or filled, ever.
    ///
    /// Fingerprints say *what* is cached; this says *which filling of it* that is. The two are not
    /// the same question, and the difference is what a writer holding a set gathered off-lock needs:
    /// an invalidate-and-refill can land byte-identical fingerprints from a different profile load,
    /// session or poll, and nothing in the contents distinguishes that from the cache never having
    /// been touched. Monotonic, bumped on every clear and every publication, and never reset.
    chain_generation: u64,
    volume_range: Option<VolumeRange>,
    // Web client state for profiles
    profiles: Vec<HqpProfile>,
    hidden_fields: HashMap<String, String>,
    config_title: Option<String>,
    digest_auth: Option<DigestAuth>,
    cookies: HashMap<String, String>,
    /// Injectable timeout/retry policy. Defaults to the shipped constants.
    timeouts: HqpTimeouts,
    /// Long-lived producer recovery policy. Separate from per-command retry semantics.
    recovery_config: HqpRecoveryConfig,
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
            endpoint_generation: 0,
            transport_generation: 0,
            connected: false,
            producer_epoch: 0,
            info: None,
            last_state: None,
            modes: Vec::new(),
            filters: Vec::new(),
            shapers: Vec::new(),
            rates: Vec::new(),
            modes_fingerprint: None,
            filters_fingerprint: None,
            shapers_fingerprint: None,
            rates_fingerprint: None,
            chain_generation: 0,
            volume_range: None,
            profiles: Vec::new(),
            hidden_fields: HashMap::new(),
            config_title: None,
            digest_auth: None,
            cookies: HashMap::new(),
            timeouts: HqpTimeouts::default(),
            recovery_config: HqpRecoveryConfig::default(),
        }
    }
}

/// HQPlayer adapter
pub struct HqpAdapter {
    state: Arc<RwLock<HqpAdapterState>>,
    connection: Arc<Mutex<Option<HqpConnection>>>,
    /// Serializes endpoint mutation with a complete semantic operation.
    ///
    /// Native conversations are already serialized one request at a time, so allowing two
    /// multi-request setting operations to overlap has no throughput benefit. A FIFO mutex keeps
    /// the root lease non-reentrant and avoids the queued-writer self-deadlock a fair `RwLock`
    /// permits when a leased public method calls another leased public method.
    operation_lock: Arc<Mutex<()>>,
    /// Serializes socket establishment. The command mutex serializes an existing connection, but
    /// without this two concurrent first requests can both observe `None` and install two sessions.
    connect_lock: Arc<Mutex<()>>,
    /// Linearizes one instance's native conversations with connection, disconnect and configure.
    ///
    /// The socket mutex protects request/reply framing, but not the identity surrounding it: without
    /// this lease `configure` can publish a new host while an old-host response is still in flight,
    /// and a poll can combine State/Status/VolumeRange around a local command.
    conversation_lock: Arc<Mutex<()>>,
    /// Set synchronously when an in-flight native future is cancelled.
    transport_poisoned: Arc<std::sync::atomic::AtomicBool>,
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
            operation_lock: Arc::new(Mutex::new(())),
            connect_lock: Arc::new(Mutex::new(())),
            conversation_lock: Arc::new(Mutex::new(())),
            transport_poisoned: Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
        let _operation_guard = self.operation_lock.lock().await;
        let _conversation_guard = self.conversation_lock.lock().await;
        let (native_changed, old_reachable) = {
            let mut state = self.state.write().await;
            let port = port.unwrap_or(DEFAULT_PORT);
            let web_port = web_port.unwrap_or(DEFAULT_WEB_PORT);
            let host_changed = state.host.as_ref() != Some(&host);
            let native_changed = host_changed || state.port != port;
            let credentials_changed = web_username
                .as_ref()
                .is_some_and(|value| state.web_username.as_ref() != Some(value))
                || web_password
                    .as_ref()
                    .is_some_and(|value| state.web_password.as_ref() != Some(value));
            let persistent_changed =
                host_changed || state.web_port != web_port || credentials_changed;
            let endpoint_changed = native_changed || persistent_changed;
            let old_reachable = native_changed.then(|| {
                (
                    state.connected,
                    state.host.clone(),
                    state.instance_name.clone(),
                )
            });
            state.host = Some(host);
            state.port = port;
            state.web_port = web_port;

            // Only update credentials if new values are provided (preserve existing)
            if web_username.is_some() {
                state.web_username = web_username;
            }
            if web_password.is_some() {
                state.web_password = web_password;
            }

            if endpoint_changed {
                state.endpoint_generation = state.endpoint_generation.wrapping_add(1);
                state.digest_auth = None;
                state.cookies.clear();
            }

            if native_changed {
                state.connected = false;
                // Clear every native-session cache when switching native endpoints.
                state.info = None;
                state.last_state = None;
                // Through the common helper rather than by hand. The lists, the fingerprints that
                // anchor them and the generation that dates them are one fact in three parts — a
                // second spelling of the clearing is how one of the three gets left behind, and a
                // fingerprint left behind is one daemon's enumeration identity used to judge whether
                // *another* daemon's chain had moved.
                Self::clear_chain_cache(&mut state);
                state.volume_range = None;
            }
            if persistent_changed {
                // Hidden fields and profile values belong to one web endpoint and credential
                // context. Reusing either against another persistent lane can submit a stale form.
                state.profiles.clear();
                state.hidden_fields.clear();
                state.config_title = None;
            }
            (native_changed, old_reachable)
        };

        if native_changed {
            let mut conn = self.connection.lock().await;
            *conn = None;
            self.transport_poisoned
                .store(false, std::sync::atomic::Ordering::Release);
        }

        if let Some((true, Some(old_host), instance_name)) = old_reachable {
            let zone_id = PrefixedZoneId::hqplayer(instance_name.as_deref().unwrap_or(&old_host));
            self.bus.publish(BusEvent::ZoneRemoved { zone_id });
            self.bus
                .publish(BusEvent::HqpDisconnected { host: old_host });
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
    ///
    /// **Skipped, not received.** A follower that arrived only partly, or one sitting complete in the
    /// connection's carry when the capture ends, is not counted until the command that consumes it runs
    /// — so a reading can lag the frames actually delivered by however many are still unconsumed. That
    /// is the honest semantics for evidence: a document is counted once, by whoever dropped it, rather
    /// than counted early and possibly twice.
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

    async fn recovery_config(&self) -> HqpRecoveryConfig {
        self.state.read().await.recovery_config
    }

    /// Override the managed producer recovery policy.
    ///
    /// This is an internal conformance seam, not a serialized setting or public control-plane
    /// contract. Production uses the supported-version-qualified default.
    pub async fn set_recovery_config(&self, config: HqpRecoveryConfig) {
        self.state.write().await.recovery_config = config;
    }

    /// Check if configured
    pub async fn is_configured(&self) -> bool {
        self.state.read().await.host.is_some()
    }

    /// Get connection status
    pub async fn get_status(&self) -> HqpConnectionStatus {
        let state = self.state.read().await;
        let poisoned = self
            .transport_poisoned
            .load(std::sync::atomic::Ordering::Acquire);
        HqpConnectionStatus {
            connected: state.connected && !poisoned,
            host: state.host.clone(),
            port: state.port,
            web_port: state.web_port,
            info: (!poisoned).then(|| state.info.clone()).flatten(),
        }
    }

    /// Identity of the most recent session that completed the coherent handshake.
    ///
    /// Internal lifecycle provenance seam for producer snapshots and conformance tests. It is not
    /// serialized by any existing HTTP, SSE or MCP payload.
    pub async fn producer_epoch(&self) -> u64 {
        self.state.read().await.producer_epoch
    }

    /// Monotonic identity of the configured native + persistent endpoint.
    ///
    /// Internal provenance seam for hermetic operation-lease tests. It is not serialized by an
    /// HTTP, SSE, adaptive-control, or MCP payload.
    pub async fn endpoint_generation(&self) -> u64 {
        self.state.read().await.endpoint_generation
    }

    /// Identity of the currently installed native transport.
    async fn transport_generation(&self) -> u64 {
        self.state.read().await.transport_generation
    }

    /// Capture the envelope fields for a coherent observation under the same native-session
    /// fence as its State, Status, volume range and chain lists. This helper deliberately reads no
    /// chain cache: the verified `ChainSnapshot` remains the only source of list data.
    async fn coherent_session_identity(
        &self,
        expected_generation: u64,
    ) -> Option<CoherentSessionIdentity> {
        let state = self.state.read().await;
        let poisoned = self
            .transport_poisoned
            .load(std::sync::atomic::Ordering::Acquire);
        if poisoned || !state.connected || state.transport_generation != expected_generation {
            return None;
        }
        Some(CoherentSessionIdentity {
            info: state.info.clone()?,
            host: state.host.clone()?,
            instance_name: state
                .instance_name
                .clone()
                .unwrap_or_else(|| "default".to_string()),
            producer_epoch: state.producer_epoch,
        })
    }

    /// Establish only the serialized native transport.
    ///
    /// The managed producer's reachability contract is deliberately stronger than this: it becomes
    /// connected only after [`Self::connect`] has assembled a coherent zone snapshot. Command retry
    /// must nevertheless be able to rebuild a socket without making an unrelated query depend on
    /// every projection field being readable. The caller holds `connect_lock`.
    async fn establish_transport_locked(&self) -> Result<()> {
        if self.connection.lock().await.is_some() {
            return Ok(());
        }

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

        // A new native session inherits nothing. It is transport-usable immediately, but it is not
        // yet a publishable producer session and therefore does not advance `producer_epoch` or set
        // `connected`.
        {
            let mut state = self.state.write().await;
            Self::clear_chain_cache(&mut state);
            state.transport_generation = state.transport_generation.wrapping_add(1);
        }
        let (read_half, write_half) = stream.into_split();
        let reader = BufReader::new(read_half);
        *self.connection.lock().await = Some(HqpConnection {
            stream: reader,
            write_half,
            carry: Vec::new(),
        });
        Ok(())
    }

    /// Connect to HQPlayer
    pub async fn connect(&self) -> Result<()> {
        let _operation_guard = self.operation_lock.lock().await;
        self.connect_with_operation_held().await
    }

    /// Complete a coherent managed handshake while the caller already owns `operation_lock`.
    ///
    /// The coherent pipeline reader uses this only after command recovery replaced the native
    /// socket during its first bounded attempt. Re-entering [`Self::connect`] there would deadlock
    /// on the operation lease; skipping the handshake would leave a transport-usable but
    /// unpublished session with no identity or producer epoch.
    async fn connect_with_operation_held(&self) -> Result<()> {
        let _conversation_guard = self.conversation_lock.lock().await;
        let _connect_guard = self.connect_lock.lock().await;
        self.reconcile_cancelled_transport().await;

        // A second caller may have completed the session while this caller waited for the guard.
        let has_connection = self.connection.lock().await.is_some();
        if has_connection && self.state.read().await.connected {
            return Ok(());
        }

        self.establish_transport_locked().await?;
        let host = self
            .state
            .read()
            .await
            .host
            .clone()
            .ok_or_else(|| anyhow!("HQPlayer host not configured"))?;

        // Reachability starts only after every field required to build a truthful direct-zone
        // snapshot has been read on this session. In particular, Status failure cannot be replaced
        // with Default: doing so publishes a stopped, empty, fixed-volume zone that the daemon never
        // reported and leaves `connected=true` after a partial handshake.
        let handshake = async {
            let info = self.get_info_inner().await?;
            let state = self.get_state_inner().await?;
            let status = self.get_playback_status_inner().await?;
            let volume_range = self.get_volume_range_inner().await?;
            Ok::<_, anyhow::Error>((info, state, status, volume_range))
        }
        .await;

        let (info, observed_state, status, volume_range) = match handshake {
            Ok(snapshot) => snapshot,
            Err(error) => {
                {
                    let mut conn = self.connection.lock().await;
                    *conn = None;
                }
                {
                    let mut state = self.state.write().await;
                    state.connected = false;
                    state.info = None;
                    state.last_state = None;
                    state.volume_range = None;
                    Self::clear_chain_cache(&mut state);
                }
                return Err(error.context("HQPlayer coherent initial snapshot failed"));
            }
        };

        {
            let mut state = self.state.write().await;
            state.info = Some(info.clone());
            state.last_state = Some(observed_state);
            state.volume_range = Some(volume_range.clone());
            state.producer_epoch = state.producer_epoch.wrapping_add(1);
            // This is deliberately last: no observer may see reachable until the session has a
            // coherent initial snapshot ready to publish.
            state.connected = true;
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
            &volume_range,
        );
        self.bus.publish(BusEvent::ZoneDiscovered { zone });

        // Lists are fetched lazily on first pipeline request via refresh_lists()
        // (Background fetch was removed due to response desync bugs - it used single-line
        // read_line() which corrupted the TCP buffer when interleaved with multi-line responses)

        Ok(())
    }

    /// Disconnect
    pub async fn disconnect(&self) {
        let _operation_guard = self.operation_lock.lock().await;
        let _conversation_guard = self.conversation_lock.lock().await;
        let (was_connected, host, instance_name) = {
            let mut state = self.state.write().await;
            let was_connected = state.connected;
            state.connected = false;
            state.info = None;
            state.last_state = None;
            state.volume_range = None;
            Self::clear_chain_cache(&mut state);
            (
                was_connected,
                state.host.clone(),
                state.instance_name.clone(),
            )
        };
        {
            let mut conn = self.connection.lock().await;
            *conn = None;
        }
        self.transport_poisoned
            .store(false, std::sync::atomic::Ordering::Release);

        if was_connected {
            let Some(ref h) = host else {
                return;
            };
            // Emit ZoneRemoved for this HQPlayer instance
            let zone_id = PrefixedZoneId::hqplayer(instance_name.as_deref().unwrap_or(h));
            self.bus.publish(BusEvent::ZoneRemoved { zone_id });

            self.bus
                .publish(BusEvent::HqpDisconnected { host: h.clone() });
        }
    }

    /// Publish a legacy direct-zone update and the adaptive producer from the same coherent
    /// native observation. The two projections may differ in shape, never in the daemon moment
    /// they claim to describe.
    async fn observe_and_publish_managed(
        &self,
        native_worker: Option<&HqpNativeWorker>,
    ) -> Result<()> {
        let snapshot = self.read_coherent_pipeline().await?;

        {
            let mut state = self.state.write().await;
            if state.producer_epoch != snapshot.producer_epoch {
                return Err(anyhow!(
                    "HQPlayer session changed between coherent pipeline observation and direct-zone publication"
                ));
            }
            state.last_state = Some(snapshot.state.clone());
            state.volume_range = Some(snapshot.volume_range.clone());
        }

        let zone = Self::hqp_status_to_zone(
            &snapshot.host,
            Some(&snapshot.instance_name),
            &snapshot.info,
            &snapshot.playback_status,
            &snapshot.volume_range,
        );
        self.bus.publish(BusEvent::ZoneDiscovered { zone });
        self.bus.publish(BusEvent::HqpStateChanged {
            host: snapshot.host.clone(),
            state: match snapshot.playback_status.state {
                0 => "stopped",
                1 => "paused",
                2 => "playing",
                _ => "unknown",
            }
            .to_string(),
        });
        self.bus.publish(BusEvent::HqpPipelineChanged {
            host: snapshot.host.clone(),
            filter: (!snapshot.playback_status.active_filter.is_empty())
                .then_some(snapshot.playback_status.active_filter.clone()),
            shaper: (!snapshot.playback_status.active_shaper.is_empty())
                .then_some(snapshot.playback_status.active_shaper.clone()),
            rate: (snapshot.playback_status.active_rate != 0)
                .then_some(snapshot.playback_status.active_rate.to_string()),
        });

        if let Some(worker) = native_worker {
            worker
                .sink
                .observed(Self::native_observation(&snapshot)?)
                .await?;
        }
        Ok(())
    }

    /// Legacy direct-zone observer retained for callers that instantiate the manager without the
    /// adaptive runtime (the compatibility test harness and standalone adapter use). Production
    /// construction uses [`Self::observe_and_publish_managed`] so the adaptive document and zone
    /// share the stricter coherent pipeline observation.
    async fn observe_and_publish_zone(&self) -> Result<()> {
        if !self.get_status().await.connected {
            self.connect().await?;
            return Ok(());
        }

        let _conversation_guard = self.conversation_lock.lock().await;
        let epoch = self.producer_epoch().await;
        let observed_state = self.get_state_inner().await?;
        let status = self.get_playback_status_inner().await?;
        let volume_range = self.get_volume_range_inner().await?;
        let (host, instance_name, info) = {
            let mut state = self.state.write().await;
            if state.producer_epoch != epoch {
                return Ok(());
            }
            state.last_state = Some(observed_state);
            state.volume_range = Some(volume_range.clone());
            (
                state
                    .host
                    .clone()
                    .ok_or_else(|| anyhow!("HQPlayer host not configured"))?,
                state.instance_name.clone(),
                state
                    .info
                    .clone()
                    .ok_or_else(|| anyhow!("HQPlayer session has no coherent identity"))?,
            )
        };
        let zone = Self::hqp_status_to_zone(
            &host,
            instance_name.as_deref(),
            &info,
            &status,
            &volume_range,
        );
        self.bus.publish(BusEvent::ZoneDiscovered { zone });
        self.bus.publish(BusEvent::HqpStateChanged {
            host: host.clone(),
            state: match status.state {
                0 => "stopped",
                1 => "paused",
                2 => "playing",
                _ => "unknown",
            }
            .to_string(),
        });
        self.bus.publish(BusEvent::HqpPipelineChanged {
            host,
            filter: (!status.active_filter.is_empty()).then_some(status.active_filter),
            shaper: (!status.active_shaper.is_empty()).then_some(status.active_shaper),
            rate: (status.active_rate != 0).then_some(status.active_rate.to_string()),
        });
        Ok(())
    }

    fn native_observation(snapshot: &CoherentPipelineSnapshot) -> Result<HqpNativeObservation> {
        let mode = snapshot
            .chain
            .modes
            .iter()
            .find(|item| item.index == u32::from(snapshot.state.mode))
            .ok_or_else(|| {
                anyhow!("HQPlayer State.mode is absent from its verified runtime modes")
            })?;
        let filter_1x_index = snapshot.state.filter1x.unwrap_or(snapshot.state.filter);
        let filter_nx_index = snapshot.state.filter_nx.unwrap_or(snapshot.state.filter);
        let filter_1x = snapshot
            .chain
            .filters
            .iter()
            .find(|item| item.index == filter_1x_index)
            .ok_or_else(|| {
                anyhow!("HQPlayer State.filter1x is absent from its verified runtime filters")
            })?;
        let filter_nx = snapshot
            .chain
            .filters
            .iter()
            .find(|item| item.index == filter_nx_index)
            .ok_or_else(|| {
                anyhow!("HQPlayer State.filterNx is absent from its verified runtime filters")
            })?;
        let shaper = snapshot
            .chain
            .shapers
            .iter()
            .find(|item| item.index == snapshot.state.shaper)
            .ok_or_else(|| {
                anyhow!("HQPlayer State.shaper is absent from its verified runtime shapers")
            })?;
        let rate = snapshot
            .chain
            .rates
            .iter()
            .find(|item| item.index == snapshot.state.rate)
            .ok_or_else(|| {
                anyhow!("HQPlayer State.rate is absent from its verified runtime rates")
            })?;

        Ok(HqpNativeObservation {
            instance_name: snapshot.instance_name.clone(),
            instance_label: Some(snapshot.info.name.clone()).filter(|name| !name.is_empty()),
            product_version: Some(snapshot.info.version.clone())
                .filter(|version| !version.is_empty()),
            producer_epoch: snapshot.producer_epoch,
            observed_at: snapshot.observed_at,
            transport: match snapshot.playback_status.state {
                0 => HqpNativeTransportState::Stopped,
                1 => HqpNativeTransportState::Paused,
                2 => HqpNativeTransportState::Playing,
                _ => HqpNativeTransportState::Unknown,
            },
            metadata: HqpNativeMetadata {
                track_id: (!snapshot.playback_status.track_id.is_empty())
                    .then_some(snapshot.playback_status.track_id.clone()),
                title: snapshot.playback_status.title.clone(),
                artist: snapshot.playback_status.artist.clone(),
                album: snapshot.playback_status.album.clone(),
            },
            volume: HqpNativeVolume {
                value_db: snapshot.state.volume_db,
                min_db: snapshot.volume_range.min_db,
                max_db: snapshot.volume_range.max_db,
                step_db: snapshot.volume_range.step_db,
                enabled: snapshot.volume_range.enabled,
                adaptive: snapshot.volume_range.adaptive,
            },
            mode_is_source: Self::mode_family(&mode.name) == Some(ModeFamily::Source),
            mode: HqpNativeSelection {
                selected: mode.name.clone(),
                choices: snapshot
                    .chain
                    .modes
                    .iter()
                    .map(|item| item.name.clone())
                    .collect(),
            },
            filter_1x: HqpNativeSelection {
                selected: filter_1x.name.clone(),
                choices: snapshot
                    .chain
                    .filters
                    .iter()
                    .map(|item| item.name.clone())
                    .collect(),
            },
            filter_nx: HqpNativeSelection {
                selected: filter_nx.name.clone(),
                choices: snapshot
                    .chain
                    .filters
                    .iter()
                    .map(|item| item.name.clone())
                    .collect(),
            },
            shaper: HqpNativeSelection {
                selected: shaper.name.clone(),
                choices: snapshot
                    .chain
                    .shapers
                    .iter()
                    .map(|item| item.name.clone())
                    .collect(),
            },
            rate: HqpNativeSelection {
                selected: rate.rate.to_string(),
                choices: snapshot
                    .chain
                    .rates
                    .iter()
                    .map(|item| item.rate.to_string())
                    .collect(),
            },
        })
    }

    /// Run this configured instance until cancelled.
    ///
    /// Each instance owns its retry timing independently, while the manager remains the single
    /// adapter-wide lifecycle/ACK visible to the coordinator. Poll futures are cancellation-safe:
    /// cancellation drops the in-flight request and immediately closes the session before return.
    async fn run_managed(
        &self,
        shutdown: CancellationToken,
        phase: SharedWorkerPhase,
        recovery: SharedRecoveryState,
        native_worker: Option<HqpNativeWorker>,
    ) {
        loop {
            let observation = tokio::select! {
                _ = shutdown.cancelled() => break,
                result = async {
                    match native_worker.as_ref() {
                        Some(worker) => self.observe_and_publish_managed(Some(worker)).await,
                        None => self.observe_and_publish_zone().await,
                    }
                } => result,
            };

            let delay = match observation {
                Ok(()) => {
                    *phase.write().await = HqpWorkerPhase::Healthy;
                    let mut recovery = recovery.lock().await;
                    if recovery.on_coherent_success(tokio::time::Instant::now()) {
                        tracing::info!(
                            "HQPlayer managed observation remained coherent for {:?}; \
                             retry debt reset",
                            recovery.config().stable_threshold
                        );
                    }
                    recovery.config().poll_interval
                }
                Err(error) => {
                    tracing::warn!("HQPlayer managed observation failed: {error}");
                    *phase.write().await = HqpWorkerPhase::Recovering;
                    if let Some(worker) = native_worker.as_ref() {
                        if let Err(sink_error) = worker
                            .sink
                            .transient_failure(&worker.instance_name, SystemTime::now())
                            .await
                        {
                            tracing::warn!(%sink_error, "HQPlayer native observation sink could not retain failed observation");
                        }
                    }
                    let delay = recovery
                        .lock()
                        .await
                        .on_failure(tokio::time::Instant::now());
                    // A handshake failure already leaves a clean disconnected session. A failure
                    // after reachability normally passes through send_command/mark_disconnected;
                    // this closes any remaining partially-used socket without publishing a false
                    // disconnect edge for a session that never became reachable.
                    self.disconnect().await;
                    delay
                }
            };

            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = tokio::time::sleep(delay) => {}
            }
        }

        self.disconnect().await;
    }

    /// Drop every cached chain-scoped enumeration.
    ///
    /// Called when the loaded chain has moved, when a mode write has reloaded it, and on connect and
    /// disconnect — a reconnected session may be talking to a restarted daemon, a different profile,
    /// or the same daemon after the source moved the chain while UHC was away, and a list index from
    /// the previous session names a **different** setting in any of those cases (HQP-C-006, HQP-C-009).
    ///
    /// `modes` is device-scoped rather than chain-scoped, so it is dropped here only because the same
    /// events that reload a chain can also be a different device.
    async fn invalidate_chain_cache(&self) {
        let mut state = self.state.write().await;
        Self::clear_chain_cache(&mut state);
    }

    /// The clearing itself, on a lock the caller already holds.
    ///
    /// Split out because [`Self::verified_chain_snapshot`] must decide *and* act inside one lock: a
    /// proof that dropped the lock to call [`Self::invalidate_chain_cache`] would be judging one
    /// cache and clearing whatever happened to be there a moment later. Every clearing in the
    /// adapter goes through here, including `configure`'s, so that the generation bump cannot be
    /// forgotten at one of them.
    ///
    /// The bump is unconditional, including when there was nothing to clear. "The cache was already
    /// empty" is not the same as "nothing happened to it": a gather in flight was reading a daemon
    /// that has since been reconnected or reconfigured, and declining to publish is the cheap,
    /// retryable outcome while publishing a set from before the event is the expensive one.
    fn clear_chain_cache(state: &mut HqpAdapterState) {
        state.modes.clear();
        state.filters.clear();
        state.shapers.clear();
        state.rates.clear();
        state.modes_fingerprint = None;
        state.filters_fingerprint = None;
        state.shapers_fingerprint = None;
        state.rates_fingerprint = None;
        Self::bump_chain_generation(state);
        tracing::debug!("Invalidated HQPlayer chain-scoped enumeration cache");
    }

    /// Record that the chain-scoped cache is no longer the filling it was.
    ///
    /// Wrapping rather than saturating: a token is compared only across one bounded gather/read, so
    /// an ABA collision would require 2^64 cache events during that operation. Saturating would be
    /// strictly worse — after reaching `MAX`, every later clear/publication would remain `MAX` and
    /// become invisible to every in-flight guard.
    fn bump_chain_generation(state: &mut HqpAdapterState) {
        state.chain_generation = state.chain_generation.wrapping_add(1);
    }

    /// The current chain-cache generation, for a caller about to gather off-lock.
    async fn chain_generation(&self) -> u64 {
        self.state.read().await.chain_generation
    }

    /// Identity of an enumeration, as `(index, label)` pairs in the order the daemon served them.
    ///
    /// Both halves matter: a chain that offers the same names in a different order is a different
    /// chain for every purpose a list index is used for.
    fn fingerprint<'a>(entries: impl Iterator<Item = (u32, &'a str)>) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for (index, label) in entries {
            index.hash(&mut hasher);
            label.hash(&mut hasher);
        }
        hasher.finish()
    }

    /// Offer **one** freshly fetched family to the cache — or refuse it, and tell the caller so.
    ///
    /// The single-family counterpart of [`Self::publish_chain_snapshot`], and the same three
    /// decisions taken in the same place: is this fetch still current, did the family move, and what
    /// gets written. `since` is the generation the fetch was issued against, captured before the
    /// request went out, so the window this covers is the whole round trip.
    ///
    /// **Under one write lock, and that is the substance of it rather than a detail of it.** These
    /// decisions used to be three separate lock acquisitions — a read to fetch the previous
    /// fingerprint, an `invalidate_chain_cache` if it had moved, a write to assign — and between any
    /// two of them a profile load, a reconnect or another poll's refresh could publish a complete
    /// set. Two distinct outcomes followed. If the replacement landed before the fingerprint read,
    /// the comparison found the *winner's* fingerprint, disagreed with it, and dropped the winner to
    /// install one family: a partial cache. If it landed between the read and the write, nothing
    /// disagreed with anything and the write simply overwrote one family of the winner's set,
    /// leaving three families from the new profile beside one from the old — **complete, coherent to
    /// every presence check, and agreeing with a rate probe whenever the two profiles offer the same
    /// rates, which is ordinary** (HQP-C-021). Held under one lock, neither interleaving exists;
    /// checked against `since`, neither outcome is reachable even so.
    ///
    /// **An overtaken fetch is an `Err`, not a quiet `Ok`.** The list does not merely fail to reach
    /// the cache — it must not reach the *caller* either. Every `fresh_*` hands its list back to a
    /// setter that resolves a name against it and sends the resulting **position** to the daemon
    /// (HQP-C-001). A position in a list the daemon has stopped serving names a different setting,
    /// which is this issue. So the loser reports its loss, the caller gets an error it can retry,
    /// and — the same asymmetry [`Self::verified_chain_snapshot`] applies to `Replaced` — the winner
    /// is not touched.
    ///
    /// **A family that reads the same is not a cache event.** No clearing, no change, no bump: the
    /// generation is what in-flight gathers and reads compare against, so bumping it for a
    /// confirmation would invalidate their work and manufacture the very race it exists to detect —
    /// and the ordinary outcome of a setter's refetch is exactly the list already cached. Contents
    /// are compared whole rather than by fingerprint alone, because the fingerprint covers
    /// `(index, name)` and a write can be a no-op only if *nothing* in the family differs. A family
    /// with no previous fingerprint is a family being filled, never a confirmation.
    ///
    /// The fingerprint is derived from the contents here, never accepted from the caller, for the
    /// reason [`Self::publish_chain_snapshot`] derives its four.
    async fn publish_fresh_family(&self, fresh: FreshFamily, since: u64) -> Result<()> {
        let fingerprint = fresh.fingerprint();
        let mut state = self.state.write().await;

        if state.chain_generation != since {
            // A full refresh can win while this single-family request is in flight. The generation
            // says the fetch may no longer be trusted *by default*, but when the winner is anchored
            // to the same routing fingerprint and the complete semantic mapping agrees element for
            // element, resolving through the reply selects exactly what resolving through the
            // winner would select. Accept the reply without publishing it or changing the winner's
            // generation. Any changed mapping remains a hard error below.
            if fresh.cached_fingerprint(&state) == Some(fingerprint)
                && fresh.resolves_like_cache(&state)
            {
                return Ok(());
            }
            return Err(anyhow!(
                "HQPlayer's {} list was read against a set of lists that has since been cleared or \
                 replaced, so the positions in it are no longer this daemon's; refusing to cache it \
                 or to resolve a setting through it",
                fresh.label()
            ));
        }

        let previous = fresh.cached_fingerprint(&state);

        // The daemon confirmed what is already there. Nothing happened to the cache, so nothing is
        // recorded as having happened to it.
        if previous == Some(fingerprint) && fresh.matches_cache(&state) {
            return Ok(());
        }

        // This family moved, so every other chain-scoped list is the previous chain's and goes.
        // Refetching them here would put a second round trip on a control path; the next read
        // refills lazily.
        if previous.is_some_and(|p| p != fingerprint) {
            tracing::info!(
                "HQPlayer {} enumeration changed: the loaded chain moved, dropping every \
                 chain-scoped list",
                fresh.label()
            );
            Self::clear_chain_cache(&mut state);
        }

        fresh.install(&mut state, fingerprint);
        Self::bump_chain_generation(&mut state);
        Ok(())
    }

    /// Fetch the modes list from the daemon and cache it, noticing a device change.
    async fn fresh_modes(&self) -> Result<Vec<ListItem>> {
        let since = self.chain_generation().await;
        let modes = self.get_modes().await?;
        self.publish_fresh_family(FreshFamily::Modes(modes.clone()), since)
            .await?;
        Ok(modes)
    }

    async fn fresh_modes_with_generation(&self) -> Result<(Vec<ListItem>, u64)> {
        let since = self.chain_generation().await;
        let (modes, generation) = self.get_modes_with_generation().await?;
        self.publish_fresh_family(FreshFamily::Modes(modes.clone()), since)
            .await?;
        Ok((modes, generation))
    }

    /// Fetch the loaded chain's filter list from the daemon and cache it.
    async fn fresh_filters(&self) -> Result<Vec<FilterItem>> {
        let since = self.chain_generation().await;
        let filters = self.get_filters().await?;
        self.publish_fresh_family(FreshFamily::Filters(filters.clone()), since)
            .await?;
        Ok(filters)
    }

    async fn fresh_filters_with_generation(&self) -> Result<(Vec<FilterItem>, u64)> {
        let since = self.chain_generation().await;
        let (filters, generation) = self.get_filters_with_generation().await?;
        self.publish_fresh_family(FreshFamily::Filters(filters.clone()), since)
            .await?;
        Ok((filters, generation))
    }

    /// Fetch the loaded chain's shaper list from the daemon and cache it.
    async fn fresh_shapers(&self) -> Result<Vec<ListItem>> {
        let since = self.chain_generation().await;
        let shapers = self.get_shapers().await?;
        self.publish_fresh_family(FreshFamily::Shapers(shapers.clone()), since)
            .await?;
        Ok(shapers)
    }

    async fn fresh_shapers_with_generation(&self) -> Result<(Vec<ListItem>, u64)> {
        let since = self.chain_generation().await;
        let (shapers, generation) = self.get_shapers_with_generation().await?;
        self.publish_fresh_family(FreshFamily::Shapers(shapers.clone()), since)
            .await?;
        Ok((shapers, generation))
    }

    /// Fetch the loaded chain's rate list from the daemon and cache it.
    ///
    /// The rate list is also what the read path and [`Self::refresh_lists`] **probe** with — through
    /// a bare [`Self::get_rates`] rather than through here, since a probe must publish nothing — for
    /// the reason this list is the chosen one: it is the smallest chain-scoped enumeration and the
    /// two chains' rate lists were observed disjoint (HQP-C-020, `E0-uhc-live`). The residual is
    /// stated rather than hidden: the offered rates also depend on the selected filter (HQP-C-021), so
    /// a filter change can move this fingerprint without the chain having moved. That direction is
    /// harmless — it costs a refetch of lists that were about to be re-read anyway. The opposite
    /// direction, a chain move that leaves the rate list byte-identical, is what would be missed, and
    /// no observation suggests it is reachable.
    async fn fresh_rates_with_generation(&self) -> Result<(Vec<RateItem>, u64)> {
        let since = self.chain_generation().await;
        let (rates, generation) = self.get_rates_with_generation().await?;
        self.publish_fresh_family(FreshFamily::Rates(rates.clone()), since)
            .await?;
        Ok((rates, generation))
    }

    /// Fingerprint of a name-keyed list — modes and shapers.
    ///
    /// Its own function for the reason [`Self::rates_fingerprint`] is: the cache and every semantic
    /// setter's proof compare enumerations, and two spellings of "the same enumeration" is a silent
    /// way for those two to disagree.
    fn list_fingerprint(items: &[ListItem]) -> u64 {
        Self::fingerprint(items.iter().map(|i| (i.index, i.name.as_str())))
    }

    /// Fingerprint of a filter list. Separate from [`Self::list_fingerprint`] only because
    /// [`FilterItem`] is a distinct type; the identity is the same `(index, name)` sequence.
    fn filters_fingerprint(items: &[FilterItem]) -> u64 {
        Self::fingerprint(items.iter().map(|i| (i.index, i.name.as_str())))
    }

    /// Fingerprint of a rate list. Its own function because it is compared in three places and a
    /// second spelling of it would be a silent way for two of them to disagree.
    fn rates_fingerprint(items: &[RateItem]) -> u64 {
        let labels: Vec<String> = items.iter().map(|r| r.rate.to_string()).collect();
        Self::fingerprint(
            items
                .iter()
                .zip(labels.iter())
                .map(|(r, label)| (r.index, label.as_str())),
        )
    }

    /// The identity of the **whole** cached chain-scoped set, as one comparable value.
    ///
    /// All four fingerprints, not the rate list alone. A rates-only identity answers "did the chain
    /// move" and cannot answer "is this the same set I looked at a moment ago": a profile load can
    /// replace the filters and shapers while leaving the rates untouched (which is exactly the case
    /// `a_profile_load_whose_refresh_fails_does_not_leave_the_previous_profiles_lists_in_place`
    /// covers), and that replacement is invisible to a rates comparison.
    ///
    /// And the generation, because even four fingerprints describe only the contents. An
    /// invalidate-and-refill that lands byte-identical lists is still a *different filling*, and the
    /// event behind it — a reconnect, a profile load, a mode write — is one that moves `State`. A
    /// read holding the pre-clear identity and the post-clear `State` must not have those two
    /// compare equal.
    ///
    /// `None` means the set is **not** a set: a family is empty, or a family is present without the
    /// fingerprint it was filled from, which would leave it unanchored. Presence and identity are the
    /// same question asked once here, because a partial cache has no identity to compare — which is
    /// what let an emptied cache pass for an unchanged one when the two were asked separately.
    fn chain_identity_of(state: &HqpAdapterState) -> Option<ChainIdentity> {
        if state.modes.is_empty()
            || state.filters.is_empty()
            || state.shapers.is_empty()
            || state.rates.is_empty()
        {
            return None;
        }
        Some(ChainIdentity {
            modes: state.modes_fingerprint?,
            filters: state.filters_fingerprint?,
            shapers: state.shapers_fingerprint?,
            rates: state.rates_fingerprint?,
            generation: state.chain_generation,
        })
    }

    /// [`Self::chain_identity_of`] against the live cache.
    async fn chain_identity(&self) -> Option<ChainIdentity> {
        Self::chain_identity_of(&*self.state.read().await)
    }

    /// **The read path's proof, and the thing it proves is a value, not a verdict.**
    ///
    /// A check that returns `bool` and leaves its caller to fetch the data again has proved nothing
    /// about what the caller then publishes: the cache it re-reads is a *later* cache, and a
    /// concurrent profile load, reconnect, `fresh_*` publication or [`Self::refresh_lists`] can have
    /// replaced it in between with one that is complete, self-consistent, and describes a different
    /// configuration entirely. Re-checking presence does not help — a complete replacement is present.
    /// So verification and extraction happen under **one** lock, and what comes back is the exact set
    /// that was verified.
    ///
    /// Three ways it can fail, and the difference between them decides whether the cache is dropped:
    ///
    /// * **Partial** — a family went missing, which is what a reconnect leaves behind. Nothing to
    ///   drop; the caller refills.
    /// * **Replaced** — the set is coherent and complete but is not the one this read captured. The
    ///   cache is not known to be wrong; it is simply newer than this read, and dropping it would
    ///   punish whoever published it for winning a race. This read fails; the cache stands. A refill
    ///   whose four fingerprints happen to match is `Replaced` too, on the generation: the lists
    ///   reading the same does not make the `State` observed after the clearing event belong to the
    ///   set observed before it.
    /// * **Moved** — after establishing that the current cache is still the captured set, the
    ///   daemon's live rate list contradicts it. That is the daemon disagreeing with the cache, so
    ///   the cache is wrong and goes.
    ///
    /// Classification order matters because the probe was fetched before this lock was acquired. A
    /// newer complete cache can arrive after the probe; that is `Replaced`, even if its rates differ
    /// from the old probe, and it must survive. Only when `current == captured` does the probe describe
    /// the same read generation and have authority to invalidate that cache as `Moved`.
    ///
    /// Residual, unchanged and unclosable from here: the probe establishes that the daemon's rate list
    /// now matches the one these lists were read from. A move out and back inside the window leaves it
    /// agreeing. Only a daemon-side atomic snapshot could do better, and the protocol has none.
    async fn verified_chain_snapshot(
        &self,
        captured: ChainIdentity,
        probe: &[RateItem],
    ) -> std::result::Result<ChainSnapshot, ChainProofFailure> {
        let probe_fingerprint = Self::rates_fingerprint(probe);
        let mut state = self.state.write().await;

        let Some(current) = Self::chain_identity_of(&state) else {
            return Err(ChainProofFailure::Partial);
        };

        // The probe predates this lock. If another complete set won the race, it is newer than the
        // probe and must not be invalidated merely because the old observation differs from it.
        if current != captured {
            return Err(ChainProofFailure::Replaced);
        }

        if current.rates != probe_fingerprint {
            tracing::info!(
                "HQPlayer rate enumeration changed: the loaded chain moved, dropping every \
                 chain-scoped list"
            );
            Self::clear_chain_cache(&mut state);
            return Err(ChainProofFailure::Moved);
        }

        Ok(ChainSnapshot {
            modes: state.modes.clone(),
            filters: state.filters.clone(),
            shapers: state.shapers.clone(),
            rates: state.rates.clone(),
        })
    }

    /// Fetch the volume range if it is not cached yet, and report what the cache holds afterwards.
    ///
    /// **Reconnect-capable, and therefore not something the read may do after proving coherence.** A
    /// lost reply here takes `send_command` through `mark_disconnected` and `connect`, and both
    /// invalidate every chain-scoped list because a session's indices must not outlive it. Done
    /// before the proof, that is merely a fill the read then has to do; done after it, it empties the
    /// cache the proof was about and the read publishes four empty lists as this daemon's settings.
    ///
    /// A lost reply or timeout is not the read's failure. The range is not chain-scoped and nothing
    /// joins it to a list index; a daemon that will not answer `VolumeRange` still has settings worth
    /// reporting, and the default renders as a fixed-volume device rather than as a wrong one. An
    /// explicit `result="Error"` remains terminal because the daemon authoritatively rejected the
    /// query rather than merely omitting the optional capability.
    async fn ensure_volume_range(&self) -> Result<PipelineVolumeRange> {
        {
            let state = self.state.read().await;
            if let Some(range) = state.volume_range.clone() {
                return Ok(PipelineVolumeRange::Observed {
                    range,
                    generation: state.transport_generation,
                });
            }
        }
        match self.get_volume_range_with_generation().await {
            Ok((range, generation)) => {
                let mut state = self.state.write().await;
                // Do not date a value from the previous socket as if the replacement served it.
                if state.transport_generation == generation {
                    state.volume_range = Some(range.clone());
                }
                Ok(PipelineVolumeRange::Observed { range, generation })
            }
            Err(error) if error.downcast_ref::<HqpRejected>().is_some() => Err(error),
            Err(e) => {
                tracing::debug!("HQPlayer volume range unavailable ({e}); reporting fixed volume");
                Ok(PipelineVolumeRange::FixedFallback(VolumeRange::default()))
            }
        }
    }

    /// Refresh every cached list, publishing **one coherent snapshot or none at all**.
    ///
    /// Deliberately *not* four calls to the `fresh_*` helpers. Each of those notices a chain change
    /// and invalidates the other families, so a sequence of them could clear a family it had already
    /// published a moment earlier and leave the cache holding a mix — some entries from the chain
    /// before the change and some from after. A mixed cache is worse than a stale one: a stale one is
    /// wrong in a way a fingerprint comparison can still detect, and a mixed one has already been
    /// published as a single view of a single daemon.
    ///
    /// So all four are gathered first and the write lock is taken once. If any family fails, nothing
    /// is published and the previous cache stands — a transient failure must not be able to blank half
    /// the view, which is exactly what assigning an empty vector per family used to do.
    ///
    /// **Taking the lock once does not by itself make the set coherent, and the gather is bracketed
    /// because of that.** The four replies arrive one after another, so they are four moments rather
    /// than one, and a source change landing between two of them yields a set that mixes the chains —
    /// atomic *publication* of an already-mixed set is no better than a mixed publication. So the rate
    /// list is read at both ends of the window and the set is published only if the two agree. If they
    /// do not, the chain moved during the gather, nothing is published, and the next poll re-reads
    /// from a settled daemon.
    ///
    /// Residual, stated: the bracket detects a chain that *ended* somewhere other than where it
    /// started. A change out and back within the same window would leave both probes agreeing. Nothing
    /// observed suggests that is reachable — a chain follows the source material, not a client's poll
    /// cycle — and the honest description of this guard is "the set did not straddle a visible
    /// transition", not "the four replies came from one instant", which nothing short of an atomic
    /// daemon-side snapshot could establish.
    ///
    /// **And the bracket does not make this writer safe to publish, which is the other guard.** Both
    /// probes agreeing says the *daemon's chain* held still; it says nothing about the *cache*, which
    /// this writer has not been holding a lock on for the whole gather. An invalidation and a
    /// complete refill from a new profile can land in that window, and if the two profiles' rate
    /// lists agree — ordinary, since the offered rates follow the filter (HQP-C-021) — the bracket is
    /// satisfied by a set that is now the previous profile's. So the generation is captured before
    /// the first reply is requested and re-checked under the publication lock: see
    /// [`Self::publish_chain_snapshot`], which is where the set is actually installed or dropped.
    ///
    /// **Returns whether a complete, coherent snapshot was published.** The caller has to be able to
    /// tell: a refresh that publishes nothing leaves the cache as it *is* — usually exactly as this
    /// refresh found it, and, when an overtaking writer is what stopped it, holding that writer's
    /// newer set instead. Either way, when the caller *needed* the fill — a first read after connect,
    /// or one after an invalidation — it may be left holding nothing at all. Swallowing that turned
    /// "I could not read this" into "this daemon offers no filters", which is the ambiguity every
    /// other guard here exists to refuse.
    #[must_use = "a refresh that did not settle leaves the cache empty, which is not the same as a daemon with no settings"]
    async fn refresh_lists(&self) -> bool {
        // Captured before the first reply is asked for, so the window this covers is the whole
        // gather rather than the part of it after some later moment.
        let since = self.chain_generation().await;

        let gathered = async {
            // Opening probe. The rate list is the bracket for the same reason it is the read path's
            // chain probe: smallest chain-scoped enumeration, and the two chains' rate lists were
            // observed disjoint (HQP-C-020).
            let opening = self.get_rates().await?;
            let modes = self.get_modes().await?;
            let filters = self.get_filters().await?;
            let shapers = self.get_shapers().await?;
            // Closing probe, and the rate list that gets published: it is the one inside the window's
            // far edge.
            let rates = self.get_rates().await?;
            Ok::<_, anyhow::Error>((opening, modes, filters, shapers, rates))
        }
        .await;

        let (opening, modes, filters, shapers, rates) = match gathered {
            Ok(all) => all,
            Err(e) => {
                tracing::warn!(
                    "HQPlayer list refresh incomplete ({e}); publishing nothing rather than a cache \
                     mixing one chain's entries with another's"
                );
                return false;
            }
        };

        let rates_fp = Self::rates_fingerprint(&rates);
        if Self::rates_fingerprint(&opening) != rates_fp {
            tracing::warn!(
                "HQPlayer's loaded chain moved while its lists were being read; publishing nothing \
                 rather than a set that straddles the change"
            );
            return false;
        }

        tracing::debug!(
            "Refreshed HQPlayer lists: {} modes, {} filters, {} shapers, {} rates",
            modes.len(),
            filters.len(),
            shapers.len(),
            rates.len()
        );

        self.publish_chain_snapshot(
            ChainSnapshot {
                modes,
                filters,
                shapers,
                rates,
            },
            since,
        )
        .await
    }

    /// Install a gathered set as the chain-scoped cache, **unless the cache moved on while it was
    /// being gathered**. Reports whether it was published.
    ///
    /// `since` is the generation the gather started from. A publication is a claim about the daemon
    /// as it was during the gather, and the gather happens off this lock: by the time the writer
    /// arrives here, an invalidation and a complete refill from a new profile can already have
    /// landed. Publishing anyway replaces a current set with one read before the change — and does
    /// so *invisibly*, because both sets are complete and self-consistent, and the four-family
    /// identity cannot tell "cached from the old profile" from "cached now" when the profiles happen
    /// to agree. A later read then captures the overwritten cache, probes rates that never moved,
    /// and reports the old profile's names for the new profile's `State`, with every guard passing.
    ///
    /// So the loser reports its loss and the winner is untouched. This is deliberately the *same*
    /// asymmetry [`Self::verified_chain_snapshot`] applies to `Replaced`: whoever is holding the
    /// older observation yields, and a cache that is not known to be wrong is not dropped.
    ///
    /// Fingerprints are derived here from the contents being installed rather than accepted from the
    /// caller, because "every family is fingerprinted from what is in it" is the invariant the whole
    /// identity comparison rests on, and a caller that computed one of the four from something else
    /// would break it silently.
    #[must_use = "an unpublished gather leaves the cache as it found it, and the caller wanted it filled"]
    async fn publish_chain_snapshot(&self, gathered: ChainSnapshot, since: u64) -> bool {
        let mut state = self.state.write().await;

        if state.chain_generation != since {
            tracing::warn!(
                "HQPlayer's setting lists were replaced while this refresh was reading them; \
                 keeping the newer set rather than overwriting it with one read from before the \
                 change"
            );
            return false;
        }

        state.modes_fingerprint = Some(Self::fingerprint(
            gathered.modes.iter().map(|m| (m.index, m.name.as_str())),
        ));
        state.filters_fingerprint = Some(Self::fingerprint(
            gathered.filters.iter().map(|f| (f.index, f.name.as_str())),
        ));
        state.shapers_fingerprint = Some(Self::fingerprint(
            gathered.shapers.iter().map(|s| (s.index, s.name.as_str())),
        ));
        state.rates_fingerprint = Some(Self::rates_fingerprint(&gathered.rates));
        state.modes = gathered.modes;
        state.filters = gathered.filters;
        state.shapers = gathered.shapers;
        state.rates = gathered.rates;
        Self::bump_chain_generation(&mut state);
        true
    }

    /// Ensure connection is established, reconnecting if needed
    pub async fn ensure_connected(&self) -> Result<()> {
        let _conversation_guard = self.conversation_lock.lock().await;
        self.ensure_transport().await
    }

    /// Ensure a native socket exists. The caller owns the conversation lease.
    async fn ensure_transport(&self) -> Result<()> {
        self.reconcile_cancelled_transport().await;
        // Check if already connected
        {
            let conn = self.connection.lock().await;
            if conn.is_some() {
                return Ok(());
            }
        }

        // Command recovery needs only a serialized transport. Managed reachability remains false
        // until `connect()` completes and publishes its coherent initial snapshot.
        let _connect_guard = self.connect_lock.lock().await;
        self.establish_transport_locked().await
    }

    /// Complete the async half of cancellation cleanup while the conversation lease is held.
    async fn reconcile_cancelled_transport(&self) {
        if self
            .transport_poisoned
            .load(std::sync::atomic::Ordering::Acquire)
        {
            self.mark_disconnected().await;
        }
    }

    /// Mark connection as broken (called on communication errors)
    async fn mark_disconnected(&self) {
        let (was_connected, host, instance_name) = {
            let mut state = self.state.write().await;
            let was_connected = state.connected;
            state.connected = false;
            state.info = None;
            state.last_state = None;
            state.volume_range = None;
            Self::clear_chain_cache(&mut state);
            (
                was_connected,
                state.host.clone(),
                state.instance_name.clone(),
            )
        };
        {
            let mut conn = self.connection.lock().await;
            *conn = None;
        }
        // Clear poison only after every status/cache field and the shared socket are invalid. A
        // status reader holding the state read lock can otherwise observe the old connected state
        // after poison has already gone false.
        self.transport_poisoned
            .store(false, std::sync::atomic::Ordering::Release);

        if was_connected {
            let Some(ref h) = host else {
                return;
            };
            tracing::warn!("HQPlayer connection lost to {}", h);
            // Emit ZoneRemoved for this HQPlayer instance
            let zone_id = PrefixedZoneId::hqplayer(instance_name.as_deref().unwrap_or(h));
            self.bus.publish(BusEvent::ZoneRemoved { zone_id });
            self.bus
                .publish(BusEvent::HqpDisconnected { host: h.clone() });
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
                Err(anyhow!(HqpRejected {
                    element,
                    reason: framing::root_text(response),
                }))
            }
            _ => Ok(()),
        }
    }

    /// Send command and get response with auto-reconnection
    async fn send_command(&self, xml: &str) -> Result<String> {
        let _conversation_guard = self.conversation_lock.lock().await;
        self.send_command_serialized(xml).await
    }

    /// Return a reply with the transport generation captured before releasing the conversation.
    async fn send_command_with_generation(&self, xml: &str) -> Result<(String, u64)> {
        let _conversation_guard = self.conversation_lock.lock().await;
        let response = self.send_command_serialized(xml).await?;
        let generation = self.transport_generation().await;
        Ok((response, generation))
    }

    /// Send exactly once on the native transport a semantic operation enumerated against.
    ///
    /// Unlike [`Self::send_command`], this never reconnects. Retrying an index-bearing write on a
    /// replacement socket can apply that index to a different daemon or setting universe. A caller
    /// may restart before a write; after a write attempt it must preserve ambiguity.
    async fn send_command_on_transport(
        &self,
        xml: &str,
        expected_generation: u64,
    ) -> Result<String> {
        let _conversation_guard = self.conversation_lock.lock().await;
        self.reconcile_cancelled_transport().await;
        let actual_generation = self.transport_generation().await;
        let has_connection = self.connection.lock().await.is_some();
        if !has_connection || actual_generation != expected_generation {
            return Err(HqpSessionChanged {
                expected: expected_generation,
                actual: actual_generation,
            }
            .into());
        }

        let mut request_attempted = false;
        match self
            .send_command_inner_tracking(xml, &mut request_attempted)
            .await
        {
            Ok(response) => {
                Self::check_result(&response)?;
                Ok(response)
            }
            Err(error) => {
                self.mark_disconnected().await;
                Err(error.context(if request_attempted {
                    "HQPlayer native session failed after the request was attempted"
                } else {
                    "HQPlayer native session failed before the request was attempted"
                }))
            }
        }
    }

    /// Retry one command while the caller owns the instance conversation lease.
    async fn send_command_serialized(&self, xml: &str) -> Result<String> {
        let timeouts = self.timeouts().await;
        let mut last_error = None;
        // Relative and toggling commands carry a side effect that is not the same applied twice.
        let one_shot = framing::root_element(xml)
            .map(|e| Self::is_one_shot(&e))
            .unwrap_or(false);

        for attempt in 0..timeouts.max_attempts {
            // Ensure we're connected
            if let Err(e) = self.ensure_transport().await {
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
            let mut request_attempted = false;
            match self
                .send_command_inner_tracking(xml, &mut request_attempted)
                .await
            {
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

                    // At-most-once for one-shot commands. Once the write has been *attempted* the daemon
                    // may already have applied the request: the protocol carries no request identity, so
                    // a lost reply, a lost request and a half-written request are indistinguishable from
                    // here. Retrying is not "recovering", it is choosing to skip a second track or step
                    // the volume twice on the chance that nothing happened. Failing is the honest
                    // outcome, and it is the one the user can correct; a silent double-apply is not.
                    //
                    // The flag is set before the first write, so this covers a partial write and a failed
                    // flush as well as a lost reply. Failures before that point — `ensure_connected`
                    // above, or the `Not connected` guard — are unambiguously pre-write and still retry,
                    // which is where the real recovery value is: a stale socket is discovered by writing
                    // to it. Queries are unaffected, since reading twice costs nothing.
                    if one_shot && request_attempted {
                        tracing::warn!(
                            "Not retrying one-shot HQPlayer command after a post-write failure; it may \
                             already have been applied and retrying would apply it twice"
                        );
                        break;
                    }

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

    /// Whether one application of this command differs from two.
    ///
    /// Relative (`VolumeUp`/`VolumeDown`) and sequential (`Next`/`Previous`) commands qualify: applying
    /// one twice is not the same as applying it once. Everything else the adapter sends is absolute —
    /// `Set*` writes a value, `Volume`/`VolumeMute` drive the level to an exact target (`VolumeMute` to
    /// the floor), `Play`/`Stop` name a state, queries only read — so applying it twice lands in the
    /// same place and retrying is safe.
    ///
    /// `VolumeMute` was once listed here as "toggling", but live validation against a real HQPlayer
    /// 6.0.2 Embedded daemon (issue #322) showed it is an absolute mute-to-floor and idempotent:
    /// repeated calls keep the level at the floor and never toggle back, and unmute is a separate
    /// `Volume` write. Excluding an idempotent command from retry made a mute whose reply was lost fail
    /// needlessly, so it is treated like any other absolute setter.
    fn is_one_shot(element: &str) -> bool {
        matches!(element, "Next" | "Previous" | "VolumeUp" | "VolumeDown")
    }

    /// Inner send command (without retry logic)
    async fn send_command_inner(&self, xml: &str) -> Result<String> {
        let mut attempted = false;
        self.send_command_inner_tracking(xml, &mut attempted).await
    }

    /// [`Self::send_command_inner`], reporting whether the request could have reached the daemon.
    ///
    /// `request_attempted` is set **before the first write**, not after the flush, and that placement is
    /// the whole point. `write_all` is not atomic: it can put part of the request on the stream and then
    /// error, and `flush` can fail after bytes have already left for the peer. Setting the flag after the
    /// flush therefore reported "the daemon certainly never saw this" for two cases where it may well
    /// have — and `send_command` would then retry a one-shot on that assurance.
    ///
    /// So the boundary is deliberately conservative: once a *connected* command enters the write
    /// attempt, every later failure — partial write, failed flush, timeout, EOF, malformed reply — is
    /// ambiguous and counts as possibly-applied. The only failures that are unambiguously pre-write are
    /// the ones that happen before this point: `ensure_connected` failing in the caller, and the
    /// `Not connected` guard below. Those still retry, which is where the recovery value is.
    ///
    /// The cost of being wrong in this direction is a spurious error on a command that never landed; the
    /// cost in the other direction is silently skipping two tracks. Only `send_command` needs the
    /// distinction, so the plain wrapper above stays the normal entry point.
    async fn send_command_inner_tracking(
        &self,
        xml: &str,
        request_attempted: &mut bool,
    ) -> Result<String> {
        let timeouts = self.timeouts().await;
        // Move the transport out of shared state for the complete in-flight conversation. This is
        // the cancellation guard: ordinary errors reach the caller's `mark_disconnected`, but a
        // dropped future executes no async cleanup branch. Keeping `Some(connection)` in shared
        // state in that case leaves a late reply queued for the next command — especially dangerous
        // when it has the same root element. With ownership local, cancellation drops the socket and
        // shared state remains `None`; only a fully framed reply installs it again below.
        let conn = self
            .connection
            .lock()
            .await
            .take()
            .ok_or_else(|| anyhow!("Not connected"))?;
        let mut in_flight = InFlightConnection::new(conn, self.transport_poisoned.clone());
        let conn = in_flight.connection_mut()?;

        // Past this point the daemon may have seen some or all of the request, so a one-shot command is
        // no longer safe to retry. Set before the first write rather than after the flush: neither
        // `write_all` nor `flush` guarantees that nothing reached the peer when it returns an error.
        *request_attempted = true;

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
        // Seeded with whatever the previous command read but did not consume, so a follower split
        // across reads keeps its prefix and completes here instead of being orphaned. The cap below
        // counts these bytes because they are already in `raw`.
        let mut raw: Vec<u8> = std::mem::take(&mut conn.carry);
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
        // The carry may already hold a complete document, so drain before reading. `first_pass`
        // enters the inner loop once without a socket read; every later entry follows a read.
        let mut first_pass = !raw.is_empty();

        'reading: while !complete {
            if first_pass {
                first_pass = false;
            } else {
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
                        // Hand back **only** this document, and keep everything after it for the next
                        // command. Truncating at the end alone was not enough: anything before the
                        // document travelled with it, and an attribute scope that cannot start falls
                        // back to searching the whole string, so a stray leading fragment could win.
                        //
                        // Complete followers behind it are counted here and dropped. A *partial*
                        // follower is deliberately NOT counted yet — it is not a document until it
                        // finishes, and the command that finishes it counts it.
                        if let Some((start, end)) = framing::first_document_span(response) {
                            let mut cursor = end;
                            while let Some((_, next)) =
                                framing::first_document_span(&response[cursor..])
                            {
                                skipped += 1;
                                self.unsolicited_skipped
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                // The same ceiling the leading path enforces. Counting without checking
                                // made the bound depend on arrival order: a burst ahead of the reply was
                                // refused at 256, the identical burst behind it was drained in full. A
                                // ceiling that a daemon can evade by reordering is not a ceiling.
                                if skipped > MAX_UNSOLICITED_BACKLOG {
                                    return Err(anyhow!(
                                        "Gave up after {} unsolicited documents while awaiting a {} \
                                         reply (coalesced behind it)",
                                        skipped,
                                        expected_element.unwrap_or_default()
                                    ));
                                }
                                tracing::debug!(
                                    "Dropping unsolicited HQPlayer document coalesced behind a {:?} \
                                     reply",
                                    expected_element
                                );
                                cursor += next;
                            }
                            conn.carry = raw[cursor..].to_vec();
                            // Bytes before the document belong to nothing: not a document, so not
                            // countable as a skipped one. With followers now preserved rather than
                            // orphaned this should be unreachable, which is exactly why it is worth
                            // saying out loud instead of dropping quietly — discarding unexplained
                            // bytes in silence is what made the original corruption invisible.
                            if start > 0 {
                                tracing::warn!(
                                    "Discarding {} byte(s) preceding a {:?} reply that belong to no \
                                     document; this should not be reachable",
                                    start,
                                    expected_element
                                );
                            }
                            raw.drain(..start);
                            raw.truncate(end - start);
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
        let response = String::from_utf8_lossy(&raw).trim().to_string();
        let mut slot = self.connection.lock().await;
        in_flight.restore(&mut slot);
        Ok(response)
    }

    // =========================================================================
    // Inner methods (used during connect, no auto-reconnect to avoid recursion)
    // =========================================================================

    /// Require the minimum authoritative root shape needed for a publishable session snapshot.
    ///
    /// The normal parsers remain tolerant for version-specific optional fields. The handshake is a
    /// different boundary: silently defaulting a missing required field would turn `<Status/>` into
    /// a fabricated stopped, zero-volume zone and call it reachable.
    fn require_root_attrs(response: &str, element: &str, attrs: &[&str]) -> Result<()> {
        let actual = framing::root_element(response);
        if actual.as_deref() != Some(element) {
            return Err(anyhow!(
                "Expected {element} handshake document, received {:?}",
                actual
            ));
        }
        let missing: Vec<&str> = attrs
            .iter()
            .copied()
            .filter(|attr| Self::parse_attr(response, attr).is_none())
            .collect();
        if !missing.is_empty() {
            return Err(anyhow!(
                "Incomplete {element} handshake document; missing root attribute(s): {}",
                missing.join(", ")
            ));
        }
        Ok(())
    }

    fn require_numeric_root_attrs(response: &str, element: &str, attrs: &[&str]) -> Result<()> {
        Self::require_root_attrs(response, element, attrs)?;
        let invalid: Vec<&str> = attrs
            .iter()
            .copied()
            .filter(|attr| {
                Self::parse_attr(response, attr)
                    .and_then(|value| value.parse::<f64>().ok())
                    .is_none()
            })
            .collect();
        if !invalid.is_empty() {
            return Err(anyhow!(
                "Invalid {element} handshake numeric attribute(s): {}",
                invalid.join(", ")
            ));
        }
        Ok(())
    }

    /// Get HQPlayer info (no reconnection)
    async fn get_info_inner(&self) -> Result<HqpInfo> {
        let xml = Self::build_request("GetInfo", &[]);
        let response = self.send_command_inner(&xml).await?;
        Self::require_root_attrs(
            &response,
            "GetInfo",
            &["name", "product", "version", "platform", "engine"],
        )?;

        Ok(HqpInfo {
            name: Self::parse_attr(&response, "name").unwrap_or_default(),
            product: Self::parse_attr(&response, "product").unwrap_or_default(),
            version: Self::parse_attr(&response, "version").unwrap_or_default(),
            platform: Self::parse_attr(&response, "platform").unwrap_or_default(),
            engine: Self::parse_attr(&response, "engine").unwrap_or_default(),
        })
    }

    /// Parse the authoritative `State` document.
    fn parse_state_response(response: &str) -> HqpState {
        HqpState {
            state: Self::parse_attr_u32(response, "state") as u8,
            mode: Self::parse_attr_u32(response, "mode") as u8,
            filter: Self::parse_attr_u32(response, "filter"),
            filter1x: Self::parse_attr(response, "filter1x").and_then(|s| s.parse().ok()),
            filter_nx: Self::parse_attr(response, "filterNx").and_then(|s| s.parse().ok()),
            shaper: Self::parse_attr_u32(response, "shaper"),
            rate: Self::parse_attr_u32(response, "rate"),
            volume: Self::parse_attr_i32(response, "volume"),
            volume_db: Self::parse_attr_f64(response, "volume").unwrap_or_default(),
            filter_junk: Self::parse_attr_u32(response, "filter_junk"),
            active_mode: Self::parse_attr_u32(response, "active_mode") as u8,
            active_rate: Self::parse_attr_u32(response, "active_rate"),
            invert: Self::parse_attr_bool(response, "invert"),
            convolution: Self::parse_attr_bool(response, "convolution"),
            repeat: Self::parse_attr_u32(response, "repeat") as u8,
            random: Self::parse_attr_bool(response, "random"),
            adaptive: Self::parse_attr_bool(response, "adaptive"),
            filter_20k: Self::parse_attr_bool(response, "filter_20k"),
            matrix_profile: Self::parse_attr(response, "matrix_profile").unwrap_or_default(),
        }
    }

    /// Get current state without reconnecting. Used only while establishing a new session.
    async fn get_state_inner(&self) -> Result<HqpState> {
        let xml = Self::build_request("State", &[]);
        let response = self.send_command_inner(&xml).await?;
        Self::require_numeric_root_attrs(&response, "State", &["state", "mode", "volume"])?;
        Ok(Self::parse_state_response(&response))
    }

    /// Read an attribute from a child element without letting child attributes override the root.
    fn parse_child_attr(response: &str, child: &str, attr: &str) -> Option<String> {
        let marker = format!("<{child}");
        let start = response.find(&marker)?;
        Self::parse_attr(&response[start..], attr)
    }

    /// Parse the complete playback status, including its optional metadata child.
    fn parse_status_response(response: &str) -> HqpStatus {
        HqpStatus {
            state: Self::parse_attr_u32(response, "state") as u8,
            track: Self::parse_attr_u32(response, "track"),
            track_id: Self::parse_attr(response, "track_id").unwrap_or_default(),
            position: Self::parse_attr_u32(response, "position"),
            length: Self::parse_attr_u32(response, "length"),
            volume: Self::parse_attr_i32(response, "volume"),
            volume_db: Self::parse_attr_f64(response, "volume").unwrap_or_default(),
            active_mode: Self::parse_attr(response, "active_mode").unwrap_or_default(),
            active_filter: Self::parse_attr(response, "active_filter").unwrap_or_default(),
            active_shaper: Self::parse_attr(response, "active_shaper").unwrap_or_default(),
            active_rate: Self::parse_attr_u32(response, "active_rate"),
            active_bits: Self::parse_attr_u32(response, "active_bits"),
            active_channels: Self::parse_attr_u32(response, "active_channels"),
            samplerate: Self::parse_attr_u32(response, "samplerate"),
            bitrate: Self::parse_attr_u32(response, "bitrate"),
            title: Self::parse_child_attr(response, "metadata", "song"),
            artist: Self::parse_child_attr(response, "metadata", "artist"),
            album: Self::parse_child_attr(response, "metadata", "album"),
        }
    }

    /// Get playback status (no reconnection)
    async fn get_playback_status_inner(&self) -> Result<HqpStatus> {
        let xml = Self::build_request("Status", &[("subscribe", "0")]);
        let response = self.send_command_inner(&xml).await?;
        Self::require_numeric_root_attrs(&response, "Status", &["state", "volume"])?;

        Ok(Self::parse_status_response(&response))
    }

    /// Get the volume capability without reconnecting. Used only during the coherent handshake.
    async fn get_volume_range_inner(&self) -> Result<VolumeRange> {
        let xml = Self::build_request("VolumeRange", &[]);
        let response = self.send_command_inner(&xml).await?;
        Self::require_root_attrs(
            &response,
            "VolumeRange",
            &["min", "max", "enabled", "adaptive"],
        )?;
        Self::require_numeric_root_attrs(&response, "VolumeRange", &["min", "max"])?;
        Ok(Self::parse_volume_range_response(&response))
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
        // No fallback to scanning the whole string. A scope that cannot start is the mechanism that
        // turned a stray leading fragment into silent corruption: `root_element` found the expected
        // root further along, so the reply looked legitimate, while the attribute came from the
        // orphan because it appeared first. Absent an identifiable root element there is no
        // attribute to report, and `None` is the honest answer — every caller already treats it as
        // "not present" rather than assuming a value.
        //
        // Both legitimate input shapes satisfy this: `send_command_inner` returns exactly one
        // document, and `parse_items` hands over one element at a time.
        let scope = framing::root_open_tag(xml)?;
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
        Ok(Self::parse_state_response(&response))
    }

    async fn get_state_with_generation(&self) -> Result<(HqpState, u64)> {
        let xml = Self::build_request("State", &[]);
        let (response, generation) = self.send_command_with_generation(&xml).await?;
        Ok((Self::parse_state_response(&response), generation))
    }

    /// Read State without allowing transparent reconnect to cross a semantic operation's session.
    async fn get_state_on_transport(&self, expected_generation: u64) -> Result<HqpState> {
        let xml = Self::build_request("State", &[]);
        let response = self
            .send_command_on_transport(&xml, expected_generation)
            .await?;
        Ok(Self::parse_state_response(&response))
    }

    /// Get playback status
    pub async fn get_playback_status(&self) -> Result<HqpStatus> {
        let xml = Self::build_request("Status", &[("subscribe", "0")]);
        let response = self.send_command(&xml).await?;
        Ok(Self::parse_status_response(&response))
    }

    fn parse_volume_range_response(response: &str) -> VolumeRange {
        VolumeRange {
            min: Self::parse_attr_i32(response, "min"),
            max: Self::parse_attr_i32(response, "max"),
            step: Self::parse_attr_i32(response, "step").max(1),
            enabled: Self::parse_attr_bool(response, "enabled"),
            adaptive: Self::parse_attr_bool(response, "adaptive"),
            min_db: Self::parse_attr_f64(response, "min").unwrap_or_default(),
            max_db: Self::parse_attr_f64(response, "max").unwrap_or_default(),
            step_db: Self::parse_attr_f64(response, "step"),
        }
    }

    /// Get volume range
    pub async fn get_volume_range(&self) -> Result<VolumeRange> {
        let xml = Self::build_request("VolumeRange", &[]);
        let response = self.send_command(&xml).await?;
        Ok(Self::parse_volume_range_response(&response))
    }

    async fn get_volume_range_with_generation(&self) -> Result<(VolumeRange, u64)> {
        let xml = Self::build_request("VolumeRange", &[]);
        let (response, generation) = self.send_command_with_generation(&xml).await?;
        Ok((Self::parse_volume_range_response(&response), generation))
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

    async fn get_modes_with_generation(&self) -> Result<(Vec<ListItem>, u64)> {
        let xml = Self::build_request("GetModes", &[]);
        let (response, generation) = self.send_command_with_generation(&xml).await?;
        Ok((
            Self::parse_items(&response, "ModesItem", |item| ListItem {
                index: Self::parse_attr_u32(item, "index"),
                name: Self::parse_attr(item, "name").unwrap_or_default(),
                value: Self::parse_attr_i32(item, "value"),
            }),
            generation,
        ))
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

    async fn get_filters_with_generation(&self) -> Result<(Vec<FilterItem>, u64)> {
        let xml = Self::build_request("GetFilters", &[]);
        let (response, generation) = self.send_command_with_generation(&xml).await?;
        Ok((
            Self::parse_items(&response, "FiltersItem", |item| FilterItem {
                index: Self::parse_attr_u32(item, "index"),
                name: Self::parse_attr(item, "name").unwrap_or_default(),
                value: Self::parse_attr_i32(item, "value"),
                arg: Self::parse_attr_u32(item, "arg"),
            }),
            generation,
        ))
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

    async fn get_shapers_with_generation(&self) -> Result<(Vec<ListItem>, u64)> {
        let xml = Self::build_request("GetShapers", &[]);
        let (response, generation) = self.send_command_with_generation(&xml).await?;
        Ok((
            Self::parse_items(&response, "ShapersItem", |item| ListItem {
                index: Self::parse_attr_u32(item, "index"),
                name: Self::parse_attr(item, "name").unwrap_or_default(),
                value: Self::parse_attr_i32(item, "value"),
            }),
            generation,
        ))
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

    async fn get_rates_with_generation(&self) -> Result<(Vec<RateItem>, u64)> {
        let xml = Self::build_request("GetRates", &[]);
        let (response, generation) = self.send_command_with_generation(&xml).await?;
        Ok((
            Self::parse_items(&response, "RatesItem", |item| RateItem {
                index: Self::parse_attr_u32(item, "index"),
                rate: Self::parse_attr_u32(item, "rate"),
            }),
            generation,
        ))
    }

    /// The name a modes or shapers list gives a position, for a report rather than for a decision.
    ///
    /// A position the list does not have is described as exactly that. It is never rendered as a bare
    /// number, because a number in a report reads like a value the caller could ask for again.
    fn list_name_at(items: &[ListItem], index: u32) -> String {
        items
            .iter()
            .find(|i| i.index == index)
            .map(|i| i.name.clone())
            .unwrap_or_else(|| format!("no entry at position {index}"))
    }

    /// [`Self::list_name_at`] for a filter list.
    fn filter_name_at(items: &[FilterItem], index: u32) -> String {
        items
            .iter()
            .find(|i| i.index == index)
            .map(|i| i.name.clone())
            .unwrap_or_else(|| format!("no entry at position {index}"))
    }

    /// The rate in Hz a rate list gives a position.
    fn rate_at(items: &[RateItem], index: u32) -> String {
        items
            .iter()
            .find(|i| i.index == index)
            .map(|i| {
                if i.rate == 0 {
                    "Auto".to_string()
                } else {
                    i.rate.to_string()
                }
            })
            .unwrap_or_else(|| format!("no entry at position {index}"))
    }

    /// Resolve a rate in **Hz** against the list the daemon is serving now (HQP-C-015).
    ///
    /// The requested value is a `u32` the caller supplied as Hz, so nothing here turns a string into a
    /// position: this is the Hz-keyed sibling of [`Self::resolve_filter`], not the numeric fallback
    /// HQP-C-063 records.
    fn resolve_rate(rates: &[RateItem], requested: &u32) -> Result<u32> {
        rates
            .iter()
            .find(|r| r.rate == *requested)
            .map(|r| r.index)
            .ok_or_else(|| {
                anyhow!(
                    "Rate {} is not offered for the chain this daemon has loaded",
                    requested
                )
            })
    }

    /// The mode family: `GetModes` positions against `State.mode`.
    const MODE: SemanticFamily<ListItem, str> = SemanticFamily {
        what: "mode",
        paired: false,
        refuses_source_mode: false,
        fingerprint: Self::list_fingerprint,
        resolve: Self::resolve_mode,
        selected: |s| SemanticSelection::Position(u32::from(s.mode)),
        describe: Self::list_name_at,
    };

    /// Both filter sides at once. Split and missing pairs are different observations and remain so.
    const FILTER_PAIR: SemanticFamily<FilterItem, str> = SemanticFamily {
        what: "filter",
        paired: true,
        refuses_source_mode: false,
        fingerprint: Self::filters_fingerprint,
        resolve: Self::resolve_filter,
        selected: |s| match (s.filter1x, s.filter_nx) {
            (Some(one_x), Some(nx)) if one_x == nx => SemanticSelection::Position(one_x),
            (Some(one_x), Some(nx)) => SemanticSelection::Split(one_x, nx),
            _ => SemanticSelection::Missing,
        },
        describe: Self::filter_name_at,
    };

    /// The 1x side alone.
    const FILTER_1X: SemanticFamily<FilterItem, str> = SemanticFamily {
        what: "filter1x",
        paired: false,
        refuses_source_mode: false,
        fingerprint: Self::filters_fingerprint,
        resolve: Self::resolve_filter,
        selected: |s| {
            s.filter1x
                .map_or(SemanticSelection::Missing, SemanticSelection::Position)
        },
        describe: Self::filter_name_at,
    };

    /// The Nx side alone.
    const FILTER_NX: SemanticFamily<FilterItem, str> = SemanticFamily {
        what: "filterNx",
        paired: false,
        refuses_source_mode: false,
        fingerprint: Self::filters_fingerprint,
        resolve: Self::resolve_filter,
        selected: |s| {
            s.filter_nx
                .map_or(SemanticSelection::Missing, SemanticSelection::Position)
        },
        describe: Self::filter_name_at,
    };

    /// The shaper family. On the verified corpus its two chains are the same length and every
    /// position but `none` names a different modulator in the other chain — `NS9` and `ASDM5` are
    /// both position 4 — so this family needs no synthetic profile to be wrong quietly.
    const SHAPER: SemanticFamily<ListItem, str> = SemanticFamily {
        what: "shaper",
        paired: false,
        refuses_source_mode: false,
        fingerprint: Self::list_fingerprint,
        resolve: Self::resolve_shaper,
        selected: |s| SemanticSelection::Position(s.shaper),
        describe: Self::list_name_at,
    };

    /// The rate family, whose request domain is **Hz** rather than a name. Same proof, different
    /// mapping shape.
    const RATE: SemanticFamily<RateItem, u32> = SemanticFamily {
        what: "rate",
        paired: false,
        refuses_source_mode: true,
        fingerprint: Self::rates_fingerprint,
        resolve: Self::resolve_rate,
        selected: |s| SemanticSelection::Position(s.rate),
        describe: Self::rate_at,
    };

    /// **Run a semantic setter and prove the semantic value, not the position it travelled as.**
    ///
    /// `decide` is the setter itself: it receives the position the request resolved to against a list
    /// fetched now, and answers with what it did — a no-op, a verified write, a refusal. Everything
    /// around it is the proof, and the proof is what [`SemanticFamily`] exists for.
    ///
    /// The shape is a **bracket**. The enumeration is read, the decision is taken, `State` is read,
    /// and the enumeration is read again. If the two enumeration identities agree, then no visible
    /// transition straddled the operation and the `State` observation — which sits between them —
    /// belongs to that enumeration; the request is then resolved against *that* list and compared
    /// with the position the daemon actually holds. That comparison is the semantic one. Without it,
    /// `AlreadySet` and `Applied` rest on a number compared against itself.
    ///
    /// `State` is read before the second list on purpose. Read after it, the observation would sit
    /// outside the bracket and the two identities would bound a window it was not in.
    ///
    /// **Bounded at one restart before a write.** If a session or enumeration moves while the
    /// decision is still a no-op, the operation may resolve once more against the settled universe.
    /// Once any write reached or may have reached the wire, it is never replayed: a changed session,
    /// failed proof, or moved enumeration returns [`SettingOutcome::Ambiguous`]. A replacement
    /// session cannot establish what the original session did, and the same numeric position may
    /// mean something else there.
    ///
    /// **This is not a compare-and-set.** The protocol has no atomic one: resolution and the write
    /// are separate commands and another controller can move the daemon between them (#162). An
    /// out-and-back (ABA) transition can also leave equal opening and closing fingerprints. The
    /// bracket establishes only "the enumeration did not *visibly* move across this operation and
    /// the daemon's position resolves to what was asked", which is strictly weaker than atomicity.
    ///
    /// The proof amplifies requests: an ordinary semantic write adds a closing list read, while one
    /// visible pre-write transition can perform a second no-op decision plus its proof. The restart
    /// is deliberately capped at one and never repeats a mutation.
    async fn proven_setting<T, R, Fetch, FetchFut, Decide, DecideFut>(
        &self,
        family: &SemanticFamily<T, R>,
        requested: &R,
        fetch: Fetch,
        decide: Decide,
    ) -> Result<SettingOutcome>
    where
        R: std::fmt::Display + ?Sized,
        Fetch: Fn() -> FetchFut,
        FetchFut: std::future::Future<Output = Result<(Vec<T>, u64)>>,
        Decide: Fn(u32, u64) -> DecideFut,
        DecideFut: std::future::Future<Output = Result<SettingOutcome>>,
    {
        let _operation_guard = self.operation_lock.lock().await;
        self.proven_setting_under_operation(family, requested, fetch, decide)
            .await
    }

    /// [`Self::proven_setting`] while the caller already owns the root operation lease.
    ///
    /// Kept separate because a fair non-reentrant lock plus a public-to-public nested call is a
    /// deadlock as soon as reconfiguration queues between them.
    async fn proven_setting_under_operation<T, R, Fetch, FetchFut, Decide, DecideFut>(
        &self,
        family: &SemanticFamily<T, R>,
        requested: &R,
        fetch: Fetch,
        decide: Decide,
    ) -> Result<SettingOutcome>
    where
        R: std::fmt::Display + ?Sized,
        Fetch: Fn() -> FetchFut,
        FetchFut: std::future::Future<Output = Result<(Vec<T>, u64)>>,
        Decide: Fn(u32, u64) -> DecideFut,
        DecideFut: std::future::Future<Output = Result<SettingOutcome>>,
    {
        let what = family.what;
        let requested_display = if family.paired {
            format!("1x={requested},Nx={requested}")
        } else {
            requested.to_string()
        };
        let mut unsettled: Option<String> = None;
        // Outcome vocabulary describes the whole operation, not merely its final attempt.
        // Applied/Ignored mean sent, Ambiguous means delivery was attempted, and
        // AlreadySet/Suppressed mean no send. Once any attempt may have reached the wire, the whole
        // operation can never truthfully become AlreadySet or Suppressed.
        let mut attempted_write = false;
        let mut acknowledged_write = false;

        // Two attempts: at most one pre-write restart. A mutation is never replayed.
        for _ in 0..2 {
            let (before, attempt_generation) = match fetch().await {
                Ok(before) => before,
                Err(error) if attempted_write => {
                    return Ok(SettingOutcome::Ambiguous {
                        what: what.to_string(),
                        reason: format!(
                            "an earlier attempt may have reached the wire, but the retry could not \
                             fetch the {what} enumeration ({error}); whether the daemon now holds \
                             {requested} is not established"
                        ),
                    });
                }
                Err(error) => return Err(error),
            };
            let identity = (family.fingerprint)(&before);
            let index = match (family.resolve)(&before, requested) {
                Ok(index) => index,
                Err(error) if attempted_write => {
                    return Ok(SettingOutcome::Ambiguous {
                        what: what.to_string(),
                        reason: format!(
                            "an earlier attempt may have reached the wire, but the retry could not \
                             resolve {requested} in the {what} enumeration ({error}); whether the \
                             daemon now holds it is not established"
                        ),
                    });
                }
                Err(error) => return Err(error),
            };

            let outcome = match decide(index, attempt_generation).await {
                Ok(outcome) => outcome,
                Err(error)
                    if error.downcast_ref::<HqpSessionChanged>().is_some() && !attempted_write =>
                {
                    unsettled = Some(format!(
                        "the native session changed after the {what} enumeration and before any \
                         write ({error})"
                    ));
                    continue;
                }
                Err(error) if attempted_write => {
                    return Ok(SettingOutcome::Ambiguous {
                        what: what.to_string(),
                        reason: format!(
                            "an earlier attempt may have reached the wire, but the retry's {what} \
                             decision failed ({error}); whether the daemon now holds {requested} is \
                             not established"
                        ),
                    });
                }
                Err(error) => return Err(error),
            };
            match &outcome {
                SettingOutcome::Applied | SettingOutcome::Ignored { .. } => {
                    attempted_write = true;
                    acknowledged_write = true;
                }
                // A session-fenced write returns Ambiguous only when its delivery or same-session
                // verification cannot be established. Continuing the generic proof would reconnect
                // and let a replacement daemon reinterpret the original session's result.
                SettingOutcome::Ambiguous { .. } => return Ok(outcome),
                SettingOutcome::AlreadySet => {}
                SettingOutcome::Suppressed { .. } if attempted_write => {
                    return Ok(SettingOutcome::Ambiguous {
                        what: what.to_string(),
                        reason: "an earlier attempt may have reached the wire, but the retry was \
                                 suppressed before the semantic result could be established"
                            .to_string(),
                    });
                }
                SettingOutcome::Suppressed { .. } => return Ok(outcome),
            }

            let generation_after_decision = self.transport_generation().await;
            if generation_after_decision != attempt_generation {
                if attempted_write {
                    return Ok(SettingOutcome::Ambiguous {
                        what: what.to_string(),
                        reason: format!(
                            "a {what} write reached or may have reached native session generation \
                             {attempt_generation}, but verification reached generation \
                             {generation_after_decision}; the result is not established"
                        ),
                    });
                }
                unsettled = Some(format!(
                    "the native session changed from generation {attempt_generation} to \
                     {generation_after_decision} before any {what} write"
                ));
                continue;
            }

            let mut state = match self.get_state_on_transport(attempt_generation).await {
                Ok(state) => state,
                Err(e) if attempted_write => {
                    return Ok(SettingOutcome::Ambiguous {
                        what: what.to_string(),
                        reason: format!(
                            "a {what} write reached or may have reached the wire, but same-session \
                             State verification failed ({e})"
                        ),
                    });
                }
                Err(e) => {
                    unsettled = Some(format!(
                        "the daemon's State could not be read back after the {what} decision ({e})"
                    ));
                    continue;
                }
            };
            if family.refuses_source_mode {
                let modes = match self.fresh_modes().await {
                    Ok(modes) => modes,
                    Err(error) if attempted_write => {
                        return Ok(SettingOutcome::Ambiguous {
                            what: what.to_string(),
                            reason: format!(
                                "a {what} write may have reached the wire, but the configured mode \
                                 could not be resolved during final proof ({error})"
                            ),
                        });
                    }
                    Err(error) => return Err(error),
                };
                // GetModes carries capabilities, not the configured selection. Re-read State after
                // that reconnect-capable request so a controller changing mode immediately after
                // the earlier proof reply cannot leave us proving rate through stale mode/rate
                // fields. The remaining post-State race is the protocol's non-atomic boundary.
                state = match self.get_state_on_transport(attempt_generation).await {
                    Ok(state) => state,
                    Err(error) if attempted_write => {
                        return Ok(SettingOutcome::Ambiguous {
                            what: what.to_string(),
                            reason: format!(
                                "a {what} write may have reached the wire, but State could not be \
                                 re-read after the final modes fetch ({error})"
                            ),
                        });
                    }
                    Err(error) => return Err(error),
                };
                if let Some(mode_name) = Self::configured_source_mode(&modes, &state) {
                    if attempted_write {
                        return Ok(SettingOutcome::Ambiguous {
                            what: what.to_string(),
                            reason: format!(
                                "a {what} write reached or may have reached the wire, but final \
                                 proof observed configured mode '{mode_name}', where the daemon \
                                 acknowledges rate pins and applies nothing"
                            ),
                        });
                    }
                    return Ok(SettingOutcome::Suppressed {
                        what: what.to_string(),
                        reason: format!(
                            "nothing was sent, and final proof observed configured mode \
                             '{mode_name}', where a rate pin cannot be established"
                        ),
                    });
                }
            }
            let (after, closing_generation) = match fetch().await {
                Ok(after) => after,
                Err(e) if attempted_write => {
                    return Ok(SettingOutcome::Ambiguous {
                        what: what.to_string(),
                        reason: format!(
                            "a {what} write reached or may have reached the wire, but the closing \
                             enumeration failed ({e})"
                        ),
                    });
                }
                Err(e) => {
                    unsettled = Some(format!(
                        "the {what} enumeration could not be re-read after the decision ({e})"
                    ));
                    continue;
                }
            };

            if closing_generation != attempt_generation {
                if attempted_write {
                    return Ok(SettingOutcome::Ambiguous {
                        what: what.to_string(),
                        reason: format!(
                            "a {what} write reached or may have reached native session generation \
                             {attempt_generation}, but the closing enumeration came from \
                             generation {closing_generation}; the result is not established"
                        ),
                    });
                }
                unsettled = Some(format!(
                    "the native session changed from generation {attempt_generation} to \
                     {closing_generation} during the {what} proof"
                ));
                continue;
            }

            if (family.fingerprint)(&after) != identity {
                if attempted_write {
                    return Ok(SettingOutcome::Ambiguous {
                        what: what.to_string(),
                        reason: format!(
                            "a {what} write reached or may have reached the wire, but its \
                             enumeration changed before final proof"
                        ),
                    });
                }
                tracing::info!(
                    "HQPlayer's {what} enumeration moved while {what} was being set, so the \
                     position that was written or compared no longer names what was asked for; \
                     resolving again against the list it serves now"
                );
                unsettled = Some(format!(
                    "the {what} enumeration the request was resolved against is not the one the \
                     daemon serves now, so the position that was compared does not name {requested} \
                     any more"
                ));
                continue;
            }

            let index_now = match (family.resolve)(&after, requested) {
                Ok(index) => index,
                Err(error) if attempted_write => {
                    return Ok(SettingOutcome::Ambiguous {
                        what: what.to_string(),
                        reason: format!(
                            "a write may have reached the wire, but {requested} could not be \
                             resolved in the closing {what} enumeration ({error})"
                        ),
                    });
                }
                Err(error) => return Err(error),
            };

            return match (family.selected)(&state) {
                SemanticSelection::Position(current) if current == index_now => {
                    if attempted_write {
                        Ok(SettingOutcome::Applied)
                    } else {
                        Ok(outcome)
                    }
                }
                SemanticSelection::Position(current) if acknowledged_write => {
                    let observed = (family.describe)(&after, current);
                    Ok(SettingOutcome::Ignored {
                        what: what.to_string(),
                        requested: requested_display.clone(),
                        observed: if family.paired {
                            format!("1x={observed},Nx={observed}")
                        } else {
                            observed
                        },
                    })
                }
                SemanticSelection::Position(current) => Ok(SettingOutcome::Suppressed {
                    what: what.to_string(),
                    reason: format!(
                        "nothing was sent: the no-op decision was resolved against a stale mapping; \
                         the stable semantic value is {} rather than {requested}",
                        (family.describe)(&after, current)
                    ),
                }),
                SemanticSelection::Split(one_x, nx) if acknowledged_write => {
                    Ok(SettingOutcome::Ignored {
                        what: what.to_string(),
                        requested: requested_display.clone(),
                        observed: format!(
                            "1x={},Nx={}",
                            (family.describe)(&after, one_x),
                            (family.describe)(&after, nx)
                        ),
                    })
                }
                SemanticSelection::Split(one_x, nx) => Ok(SettingOutcome::Suppressed {
                    what: what.to_string(),
                    reason: format!(
                        "nothing was sent: the no-op decision was resolved against a stale mapping; \
                         State reports the split pair 1x={},Nx={} rather than {requested}",
                        (family.describe)(&after, one_x),
                        (family.describe)(&after, nx)
                    ),
                }),
                SemanticSelection::Missing => Ok(SettingOutcome::Ambiguous {
                    what: what.to_string(),
                    reason: format!(
                        "the daemon does not report {what} as one value, so whether it now holds \
                         {requested} cannot be established from its State"
                    ),
                }),
            };
        }

        Ok(SettingOutcome::Ambiguous {
            what: what.to_string(),
            reason: format!(
                "{}, and the retry met the same thing, so whether the daemon now holds {requested} \
                 is not established; {}",
                unsettled.unwrap_or_else(|| format!("the {what} enumeration would not settle"))
                ,
                if attempted_write {
                    "at least one write reached or may have reached the wire"
                } else {
                    "no write reached the wire"
                }
            ),
        })
    }

    /// Read back an index-bearing write without allowing verification to reconnect to another
    /// native session. Once the write was attempted, losing this session is ambiguity, not a reason
    /// to inspect a replacement daemon and call that the result.
    async fn verify_applied_on_transport<T, F>(
        &self,
        expected_generation: u64,
        what: &str,
        expected: T,
        observe: F,
    ) -> Result<SettingOutcome>
    where
        T: PartialEq + std::fmt::Display,
        F: Fn(&HqpState) -> Option<T>,
    {
        let timeouts = self.timeouts().await;
        let mut last_seen: Option<T> = None;

        for attempt in 0..timeouts.max_attempts.max(1) {
            if attempt > 0 {
                tokio::time::sleep(timeouts.reconnect_delay).await;
            }
            let state = self.get_state_on_transport(expected_generation).await?;
            let seen = observe(&state);
            if seen.as_ref() == Some(&expected) {
                return Ok(SettingOutcome::Applied);
            }
            if seen.is_some() {
                last_seen = seen;
            }
        }

        match last_seen {
            Some(observed) => Ok(SettingOutcome::Ignored {
                what: what.to_string(),
                requested: expected.to_string(),
                observed: observed.to_string(),
            }),
            None => Ok(SettingOutcome::Ambiguous {
                what: what.to_string(),
                reason: format!(
                    "the daemon does not report {what}, so whether it now holds {expected} cannot \
                     be established from its State"
                ),
            }),
        }
    }

    /// Write and verify on the exact transport whose enumeration supplied the wire position.
    async fn write_setting_on_transport<T, F>(
        &self,
        expected_generation: u64,
        xml: &str,
        what: &str,
        expected: T,
        observe: F,
    ) -> Result<SettingOutcome>
    where
        T: PartialEq + std::fmt::Display + Clone,
        F: Fn(&HqpState) -> Option<T>,
    {
        match self
            .send_command_on_transport(xml, expected_generation)
            .await
        {
            Ok(_) => match self
                .verify_applied_on_transport(expected_generation, what, expected, observe)
                .await
            {
                Ok(outcome) => Ok(outcome),
                Err(error) => Ok(SettingOutcome::Ambiguous {
                    what: what.to_string(),
                    reason: format!(
                        "the write was attempted on native session generation \
                         {expected_generation}, but same-session verification failed ({error})"
                    ),
                }),
            },
            Err(error) if error.downcast_ref::<HqpRejected>().is_some() => Err(error),
            Err(error) if error.downcast_ref::<HqpSessionChanged>().is_some() => Err(error),
            Err(error) => Ok(SettingOutcome::Ambiguous {
                what: what.to_string(),
                reason: format!(
                    "the write was attempted on native session generation {expected_generation} \
                     but drew no usable reply ({error}); it may or may not have been applied, and \
                     verification was not moved to a replacement session"
                ),
            }),
        }
    }

    /// The semantic family a mode name belongs to, matched by prefix and alias rather than position.
    ///
    /// Positions are unsafe: a DAC without DSD capability yields a modes list with **no** SDM entry and
    /// the survivors keep their own indices, so a caller assuming index 2 is SDM finds something on one
    /// device and the wrong thing on another (HQP-C-013). Names are not exact either — the daemon
    /// reports `"SDM (DSD)"`, so equality against `"SDM"` or `"DSD"` fails on the device that has it.
    ///
    /// `None` means "no family recognised", which includes every string of digits. That is deliberate:
    /// a bare integer is not a mode name, and treating one as a list index is what HQP-C-063 records.
    fn mode_family(name: &str) -> Option<ModeFamily> {
        let trimmed = name.trim().to_ascii_lowercase();
        let bare = trimmed.trim_start_matches('[').trim_end_matches(']');
        if bare == "source" {
            return Some(ModeFamily::Source);
        }
        if trimmed.starts_with("pcm") {
            return Some(ModeFamily::Pcm);
        }
        if trimmed.starts_with("sdm") || trimmed.starts_with("dsd") {
            return Some(ModeFamily::Sdm);
        }
        None
    }

    /// Resolve a mode name against the list the daemon is serving **now**.
    ///
    /// Exact match first, then semantic family. A family match must be unambiguous: if a daemon ever
    /// offered two SDM entries, picking one of them silently would be the positional assumption this
    /// exists to remove, so it is refused instead.
    fn resolve_mode(modes: &[ListItem], requested: &str) -> Result<u32> {
        if let Some(exact) = modes
            .iter()
            .find(|m| m.name.eq_ignore_ascii_case(requested))
        {
            return Ok(exact.index);
        }
        let available = || {
            modes
                .iter()
                .map(|m| m.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let wanted = Self::mode_family(requested).ok_or_else(|| {
            anyhow!(
                "Mode '{}' is not a mode this daemon offers. Available: {}",
                requested,
                available()
            )
        })?;
        let mut matches = modes
            .iter()
            .filter(|m| Self::mode_family(&m.name) == Some(wanted));
        match (matches.next(), matches.next()) {
            (Some(only), None) => Ok(only.index),
            (Some(_), Some(_)) => Err(anyhow!(
                "Mode '{}' matches more than one entry this daemon offers, so no single one can be \
                 chosen without guessing. Available: {}",
                requested,
                available()
            )),
            _ => Err(anyhow!(
                "Mode '{}' is not offered by this daemon. Available: {}",
                requested,
                available()
            )),
        }
    }

    /// Set mode by semantic name — `"PCM"`, `"DSD"`, `"[source]"`, or the daemon's own `"SDM (DSD)"`.
    ///
    /// Resolved against the list the daemon serves now and sent as that list's **index** (HQP-C-001).
    ///
    /// A mode the daemon is already in is **not written**. `SetMode` clears the exact-rate pin even
    /// when the mode does not change (HQP-C-017), so an unconditional write destroys a user's pinned
    /// rate for nothing. When a write is performed the chain reloads, so every chain-scoped list is
    /// dropped and the pin the daemon just cleared is re-read rather than remembered.
    /// The chain-transition hazard [`SemanticFamily`] describes cannot reach *this* family: `GetModes`
    /// is device-scoped, so the loaded chain moving does not renumber it. What can reshape it is a
    /// device or profile change — a PCM-only DAC's list has **no** SDM entry and the survivors keep
    /// their own positions (HQP-C-013) — and an *external* one moves no UHC cache and bumps no
    /// generation, so the fence does not cover it either. An exemption argued from "this family does
    /// not move" is the shape of reasoning this issue exists to remove, so mode is proved like the
    /// rest.
    pub async fn set_mode(&self, mode_name: &str) -> Result<SettingOutcome> {
        let _operation_guard = self.operation_lock.lock().await;
        self.set_mode_under_operation(mode_name).await
    }

    async fn set_mode_under_operation(&self, mode_name: &str) -> Result<SettingOutcome> {
        self.proven_setting_under_operation(
            &Self::MODE,
            mode_name,
            || self.fresh_modes_with_generation(),
            |mode_index, generation| async move {
                let state = self.get_state_on_transport(generation).await?;
                if u32::from(state.mode) == mode_index {
                    tracing::debug!(
                        "HQPlayer is already in mode {mode_index}; not writing it, because SetMode \
                         clears the rate pin even when the mode does not change"
                    );
                    return Ok(SettingOutcome::AlreadySet);
                }

                let xml = Self::build_request("SetMode", &[("value", &mode_index.to_string())]);
                let outcome = self
                    .write_setting_on_transport(generation, &xml, "mode", mode_index, |s| {
                        Some(u32::from(s.mode))
                    })
                    .await?;
                // A mode change reloads the chain whatever the readback said, so nothing cached
                // about it survives. This is also what makes the cleared pin honest: there is no
                // remembered rate to report, only the one the daemon now holds.
                self.invalidate_chain_cache().await;
                Ok(outcome)
            },
        )
        .await
    }

    /// The request both sides of the pair travel in, built in one place so they cannot drift.
    fn filter_request(value_nx: u32, value1x: u32) -> String {
        let nx = value_nx.to_string();
        let one_x = value1x.to_string();
        Self::build_request("SetFilter", &[("value", &nx), ("value1x", &one_x)])
    }

    /// The observable both sides of the pair are verified against, as one value.
    ///
    /// The pair is what was requested, so reporting half of it back would be the same mistake as
    /// writing half of it. `None` when the daemon reports neither sibling — `verify_applied` turns an
    /// absent field into `Ambiguous`, which is what "nothing can be established" honestly is.
    fn filter_pair_observation(state: &HqpState) -> Option<String> {
        match (state.filter1x, state.filter_nx) {
            (Some(one_x), Some(nx)) => Some(format!("1x={one_x},Nx={nx}")),
            _ => None,
        }
    }

    /// Send `SetFilter` with **both** arguments, by index.
    ///
    /// The daemon's `SetFilter` writes both sides: `value` alone sets 1x and Nx together, and
    /// `value1x` splits them. There is therefore no such thing as a one-sided write on the wire — a
    /// caller that omits the sibling is overwriting it with whatever it passed as `value`. Taking both
    /// indices is what makes that impossible to express, rather than merely discouraged.
    ///
    /// **Indices in — and that is the only thing this form gives up.** It answered `Ok(())` on
    /// `result="OK"` alone until an independent review pointed out that a public method doing so is a
    /// false success whatever it is called, so it is verified like every other write: `OK` is not
    /// proof of application here any more than anywhere else (HQP-C-028).
    ///
    /// Every advertised surface goes through [`Self::set_filter_1x`], [`Self::set_filter_nx`] or
    /// [`Self::set_filter_pair`], which additionally resolve a semantic name against the loaded
    /// chain's list. This form exists for a caller that already holds indices — the conformance
    /// suite's cross-lane checks, which feed a persistent-domain enum ID to the live lane and require
    /// it to be refused.
    pub async fn set_filter(&self, value_nx: u32, value1x: u32) -> Result<SettingOutcome> {
        let _operation_guard = self.operation_lock.lock().await;
        let (_, generation) = self.get_state_with_generation().await?;
        tracing::debug!("SetFilter: value={value_nx} (Nx), value1x={value1x} (1x)");
        let expected = format!("1x={value1x},Nx={value_nx}");
        let xml = Self::filter_request(value_nx, value1x);
        self.write_setting_on_transport(
            generation,
            &xml,
            "filter",
            expected,
            Self::filter_pair_observation,
        )
        .await
    }

    /// Set only the 1x filter, preserving the Nx filter the daemon reports.
    pub async fn set_filter_1x(&self, filter_name: &str) -> Result<SettingOutcome> {
        self.set_filter_side(FilterSide::OneX, filter_name).await
    }

    /// Set only the Nx filter, preserving the 1x filter the daemon reports.
    pub async fn set_filter_nx(&self, filter_name: &str) -> Result<SettingOutcome> {
        self.set_filter_side(FilterSide::Nx, filter_name).await
    }

    /// Set **both** filter sides to the same name, in one `SetFilter`.
    ///
    /// The legacy `POST /hqplayer/setting` route's `filter` setting means "both sides". Doing that as
    /// two one-sided writes can half-apply — the 1x write lands and the Nx write is rejected, ignored,
    /// or lost — leaving the daemon on a pair the caller never asked for and no single outcome that
    /// describes it. One `SetFilter` carrying both indices cannot half-apply on the wire.
    ///
    /// It also needs no sibling: both sides are being written, so nothing has to be read out of
    /// `State` first and the refusal in [`Self::set_filter_side`] does not apply here.
    pub async fn set_filter_pair(&self, filter_name: &str) -> Result<SettingOutcome> {
        let _operation_guard = self.operation_lock.lock().await;
        self.set_filter_pair_under_operation(filter_name).await
    }

    async fn set_filter_pair_under_operation(&self, filter_name: &str) -> Result<SettingOutcome> {
        self.proven_setting_under_operation(
            &Self::FILTER_PAIR,
            filter_name,
            || self.fresh_filters_with_generation(),
            |index, generation| async move {
                let state = self.get_state_on_transport(generation).await?;
                if state.filter1x == Some(index) && state.filter_nx == Some(index) {
                    return Ok(SettingOutcome::AlreadySet);
                }

                // Both sides are the setting here, so both are verified. Rendered as one value
                // because the pair is what was requested and reporting half of it back would be the
                // same mistake as writing half of it.
                let expected = format!("1x={index},Nx={index}");
                tracing::debug!("SetFilter (both sides): value={index}, value1x={index}");
                let xml = Self::filter_request(index, index);
                self.write_setting_on_transport(
                    generation,
                    &xml,
                    "filter",
                    expected,
                    Self::filter_pair_observation,
                )
                .await
            },
        )
        .await
    }

    /// One side of the filter pair, carrying the authoritative other side with it.
    ///
    /// The sibling comes from `State`'s own `filter1x`/`filterNx` and from nowhere else. The previous
    /// implementation substituted the legacy combined `filter` field when a sibling was absent, which
    /// is a **guess**: `filter` tracks the most recently set of the two, so the guess silently
    /// overwrote the setting the caller did not touch. When the sibling cannot be established the
    /// write is refused with a reason and nothing is sent.
    async fn set_filter_side(&self, side: FilterSide, filter_name: &str) -> Result<SettingOutcome> {
        let _operation_guard = self.operation_lock.lock().await;
        self.set_filter_side_under_operation(side, filter_name)
            .await
    }

    async fn set_filter_side_under_operation(
        &self,
        side: FilterSide,
        filter_name: &str,
    ) -> Result<SettingOutcome> {
        let family = match side {
            FilterSide::OneX => &Self::FILTER_1X,
            FilterSide::Nx => &Self::FILTER_NX,
        };
        let what = side.field();
        self.proven_setting_under_operation(
            family,
            filter_name,
            || self.fresh_filters_with_generation(),
            |index, generation| async move {
                let state = self.get_state_on_transport(generation).await?;
                let (Some(current_1x), Some(current_nx)) = (state.filter1x, state.filter_nx) else {
                    return Ok(SettingOutcome::Suppressed {
                        what: what.to_string(),
                        reason: "the daemon's State does not report both filter1x and filterNx, and \
                                 SetFilter writes both sides at once; sending it would overwrite the \
                                 sibling with a guess"
                            .to_string(),
                    });
                };

                let current = match side {
                    FilterSide::OneX => current_1x,
                    FilterSide::Nx => current_nx,
                };
                if current == index {
                    return Ok(SettingOutcome::AlreadySet);
                }

                let (send_nx, send_1x) = match side {
                    FilterSide::OneX => (current_nx, index),
                    FilterSide::Nx => (index, current_1x),
                };
                tracing::debug!("SetFilter: value={send_nx} (Nx), value1x={send_1x} (1x)");
                let xml = Self::filter_request(send_nx, send_1x);
                self.write_setting_on_transport(generation, &xml, what, index, |s| match side {
                    FilterSide::OneX => s.filter1x,
                    FilterSide::Nx => s.filter_nx,
                })
                .await
            },
        )
        .await
    }

    /// Resolve a filter name against the list the daemon is serving **now**.
    ///
    /// No numeric fallback. A list index that arrives as a name is not a name — it is a number from
    /// some other chain, some other daemon, or a guess, and accepting it is how a stale index silently
    /// selects a different filter (HQP-C-009, HQP-C-063).
    fn resolve_filter(filters: &[FilterItem], requested: &str) -> Result<u32> {
        filters
            .iter()
            .find(|f| f.name == requested)
            .or_else(|| {
                filters
                    .iter()
                    .find(|f| f.name.eq_ignore_ascii_case(requested))
            })
            .map(|f| f.index)
            .ok_or_else(|| {
                anyhow!(
                    "Filter '{}' is not in the list this daemon is serving for the chain it has \
                     loaded",
                    requested
                )
            })
    }

    /// Resolve a shaper name against the list the daemon is serving **now**. No numeric fallback,
    /// for the same reason as [`Self::resolve_filter`].
    fn resolve_shaper(shapers: &[ListItem], requested: &str) -> Result<u32> {
        shapers
            .iter()
            .find(|s| s.name == requested)
            .or_else(|| {
                shapers
                    .iter()
                    .find(|s| s.name.eq_ignore_ascii_case(requested))
            })
            .map(|s| s.index)
            .ok_or_else(|| {
                anyhow!(
                    "Shaper '{}' is not in the list this daemon is serving for the chain it has \
                     loaded",
                    requested
                )
            })
    }

    /// Set the shaper (the modulator, under an SDM chain) by semantic name.
    pub async fn set_shaper(&self, shaper_name: &str) -> Result<SettingOutcome> {
        let _operation_guard = self.operation_lock.lock().await;
        self.set_shaper_under_operation(shaper_name).await
    }

    async fn set_shaper_under_operation(&self, shaper_name: &str) -> Result<SettingOutcome> {
        self.proven_setting_under_operation(
            &Self::SHAPER,
            shaper_name,
            || self.fresh_shapers_with_generation(),
            |shaper_index, generation| async move {
                if self.get_state_on_transport(generation).await?.shaper == shaper_index {
                    return Ok(SettingOutcome::AlreadySet);
                }

                let xml =
                    Self::build_request("SetShaping", &[("value", &shaper_index.to_string())]);
                self.write_setting_on_transport(generation, &xml, "shaper", shaper_index, |s| {
                    Some(s.shaper)
                })
                .await
            },
        )
        .await
    }

    /// Whether the daemon's **configured** mode is the source-following one.
    ///
    /// Read from the modes list the daemon is serving now, by semantic family — never from a list
    /// position, and never from `State.active_mode`, whose meaning under `[source]` is unmeasured
    /// (HQP-C-024). Returns the daemon's own name for the mode so a refusal can quote it.
    fn configured_source_mode(modes: &[ListItem], state: &HqpState) -> Option<String> {
        modes
            .iter()
            .find(|m| m.index == u32::from(state.mode))
            .filter(|m| Self::mode_family(&m.name) == Some(ModeFamily::Source))
            .map(|m| m.name.clone())
    }

    /// Pin the sample rate, in Hz.
    ///
    /// The Hz value is resolved to the **list index** the loaded chain gives it (HQP-C-015), against a
    /// list fetched now rather than a cached one — the offered rates change wholesale with the chain
    /// (HQP-C-020).
    ///
    /// Under a configured `[source]` mode the write is **suppressed and never sent**. The daemon
    /// answers `OK` there and applies nothing (HQP-C-018); what governs the rate in that mode is a
    /// persistent configuration limit for which no wire command exists, so no retry can succeed. The
    /// Auto case is why suppression rather than readback is the answer: requesting index 0 when the
    /// rate is already 0 compares 0 against 0, so a readback reports success for a command that did
    /// nothing (HQP-C-019).
    pub async fn set_rate(&self, rate_value: u32) -> Result<SettingOutcome> {
        // The pin is a name-to-position problem like every other setter's, so it is proved like one.
        // Source-mode suppression lives inside the decision so every retry re-evaluates it before a
        // write; checking only once outside the bracket would let a transition to `[source]` during
        // the first attempt send a pin on the retry.
        self.proven_setting(
            &Self::RATE,
            &rate_value,
            || self.fresh_rates_with_generation(),
            |index, generation| async move {
                let modes = self.fresh_modes().await?;
                let state = self.get_state_on_transport(generation).await?;
                if let Some(mode_name) = Self::configured_source_mode(&modes, &state) {
                    return Ok(SettingOutcome::Suppressed {
                        what: "rate".to_string(),
                        reason: format!(
                            "the configured mode is '{mode_name}', in which the daemon acknowledges \
                             a rate pin and applies nothing — the rate is governed by persistent \
                             configuration there, so the write was not sent"
                        ),
                    });
                }
                if state.rate == index {
                    return Ok(SettingOutcome::AlreadySet);
                }

                let xml = Self::build_request("SetRate", &[("value", &index.to_string())]);
                self.write_setting_on_transport(generation, &xml, "rate", index, |s| {
                    Some(s.rate)
                })
                .await
            },
        )
        .await
    }

    /// Turn a legacy numeric setting value into the semantic name it denotes **right now**.
    ///
    /// `POST /hqplayer/setting` carries `value: u32` and `POST /hqp/pipeline` accepts a JSON number,
    /// and both request contracts are frozen. So the number is kept — at this boundary, and only
    /// here. It is resolved against the enumeration the daemon is serving for the chain it has
    /// loaded, and what continues inward is the **name**.
    ///
    /// That is the whole of HQP-C-063's fix. The number never becomes an identity: it is not cached,
    /// not published, and not passed to a setter. A position that the current list does not have is
    /// an error rather than something forwarded and hoped about — which is what the removed
    /// `parse::<u32>()` fallback did, and why a stale number could select a different setting.
    pub async fn legacy_index_to_name(
        &self,
        family: LegacySettingFamily,
        index: u32,
    ) -> Result<String> {
        let _operation_guard = self.operation_lock.lock().await;
        self.legacy_index_to_name_under_operation(family, index)
            .await
    }

    async fn legacy_index_to_name_under_operation(
        &self,
        family: LegacySettingFamily,
        index: u32,
    ) -> Result<String> {
        let (name, count) = match family {
            LegacySettingFamily::Mode => {
                let modes = self.fresh_modes().await?;
                (
                    modes
                        .iter()
                        .find(|m| m.index == index)
                        .map(|m| m.name.clone()),
                    modes.len(),
                )
            }
            LegacySettingFamily::Filter => {
                let filters = self.fresh_filters().await?;
                (
                    filters
                        .iter()
                        .find(|f| f.index == index)
                        .map(|f| f.name.clone()),
                    filters.len(),
                )
            }
            LegacySettingFamily::Shaper => {
                let shapers = self.fresh_shapers().await?;
                (
                    shapers
                        .iter()
                        .find(|s| s.index == index)
                        .map(|s| s.name.clone()),
                    shapers.len(),
                )
            }
        };
        name.ok_or_else(|| {
            anyhow!(
                "{:?} position {} is not in the {}-entry list this daemon is serving now",
                family,
                index,
                count
            )
        })
    }

    /// Resolve and apply one frozen numeric setting contract under one endpoint operation lease.
    ///
    /// Keeping this bracket inside the adapter prevents a handler from resolving endpoint A's list
    /// position, releasing the lease, and then invoking a separately leased named setter on B.
    pub async fn apply_legacy_index(
        &self,
        target: LegacySettingTarget,
        index: u32,
    ) -> Result<SettingOutcome> {
        let _operation_guard = self.operation_lock.lock().await;
        let name = self
            .legacy_index_to_name_under_operation(target.family(), index)
            .await?;
        match target {
            LegacySettingTarget::Mode => self.set_mode_under_operation(&name).await,
            LegacySettingTarget::FilterPair => self.set_filter_pair_under_operation(&name).await,
            LegacySettingTarget::Filter1x => {
                self.set_filter_side_under_operation(FilterSide::OneX, &name)
                    .await
            }
            LegacySettingTarget::FilterNx => {
                self.set_filter_side_under_operation(FilterSide::Nx, &name)
                    .await
            }
            LegacySettingTarget::Shaper => self.set_shaper_under_operation(&name).await,
        }
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

    /// Get the legacy pipeline payload from one coherent native observation.
    pub async fn get_pipeline_status(&self) -> Result<PipelineStatus> {
        Ok(self.read_coherent_pipeline().await?.legacy)
    }

    /// Read the one native observation from which both the legacy pipeline payload and the
    /// adaptive producer document are derived.
    ///
    /// This is deliberately the only owner of the generation-fenced gather. A second read of
    /// `state`, cache or status after its proof would make the adaptive document a mixture of two
    /// daemon moments even if the legacy response still looked plausible.
    async fn read_coherent_pipeline(&self) -> Result<CoherentPipelineSnapshot> {
        if !self.get_status().await.connected {
            self.connect().await?;
        }
        let _operation_guard = self.operation_lock.lock().await;
        // `State` reports settings as **list indices**, and an index means nothing apart from the list
        // it came from. So this read has one job beyond fetching: to establish that the `State` it
        // publishes and the lists it resolves that state through describe **the same loaded chain**,
        // and to publish exactly the values that establishment covered.
        //
        // Reading `State` first — as this once did — is the same misselection this issue exists to
        // end, arriving through the read path and needing no write at all: under `[source]` the chain
        // moves without `State.mode` moving (HQP-C-007), so a pre-transition index was resolved
        // against post-transition lists.
        //
        // The order below is the whole design, and each step is where it is for a reason:
        //
        // 1. **The volume range first**, because fetching it can reconnect, and a reconnect empties
        //    every chain-scoped list. Anything reconnect-capable has to happen before coherence is
        //    established, never after it — a proof followed by a reconnect is a proof about a cache
        //    that no longer exists. It used to run last, between the check and the publication, which
        //    made four empty lists the *deterministic* answer to a lost `VolumeRange` reply on the
        //    first read of a connection.
        // 2. **Fill if needed and capture the identity** of all four families — fingerprints, not
        //    contents — before reading `State`. Four and not just the rates: a profile load can
        //    replace filters and shapers while leaving the rate list untouched, and a rates-only
        //    identity cannot see it. A required fill that does not settle is the read's failure, not
        //    a quieter version of success.
        // 3. **`State` and `Status`.**
        // 4. **Probe the daemon's rate list**, which is the chain-movement check: smallest
        //    chain-scoped enumeration, and the two chains' were observed disjoint (HQP-C-020).
        // 5. **Prove and take the snapshot under one lock**, and project from *that* — never from a
        //    later read of the cache. A verdict plus a re-read is two moments, and a concurrent
        //    profile load, reconnect, `fresh_*` publication or refresh can replace the cache between
        //    them with a complete, coherent, entirely different set. Presence cannot distinguish that
        //    case; identity can.
        //
        // One retry for the whole read, not per step: a daemon whose chain is flapping would
        // otherwise hold this open. A first unsettled arm explicitly continues; a proven snapshot
        // breaks; an exhausted arm returns an error. There is no third attempt or fall-through read.
        //
        // **Residual, and it cannot be closed from here.** The probe confirms the chain is the same
        // *now* as when the lists were published; a move out and back inside that window leaves it
        // agreeing. Only a daemon-side atomic snapshot could do better, and the protocol has none.
        let mut retried = false;
        let (state, playback_status, snapshot, vol_range, session) = loop {
            // Query recovery may replace only the TCP transport and deliberately leave managed
            // reachability false. Any first-attempt disturbance can discover that replacement —
            // an unsettled list fill, failed probe, replaced snapshot, or the closing fence — so
            // the one whole-read retry must complete a coherent handshake here rather than only in
            // one of those branches.
            if retried && !self.get_status().await.connected {
                self.connect_with_operation_held().await.context(
                    "HQPlayer replacement transport could not complete the coherent handshake \
                     required before re-reading its setting lists",
                )?;
            }

            // (1) Reconnect-capable, so it goes ahead of everything the proof will cover. On the
            // first read of a connection this is always a real request.
            let (vol_range, attempt_generation) = match self.ensure_volume_range().await? {
                PipelineVolumeRange::Observed { range, generation } => (range, generation),
                PipelineVolumeRange::FixedFallback(range) => {
                    // A timeout/lost reply discarded the old socket. Establish the replacement
                    // before dating the complete pipeline attempt, so its later generation fence
                    // covers the session that serves the setting lists and state rather than the
                    // failed session whose optional volume capability is being replaced.
                    self.ensure_connected().await?;
                    (range, self.transport_generation().await)
                }
            };

            // (2) asks one question: *is there a complete set, and what is its identity*. Lazy-fill
            // only if there is not — on the first read after connect
            // that is all of it — and then ask again rather than assuming the fill settled, since a
            // reconnect can empty the cache between publishing it and looking at it. A required fill
            // that does not settle is the read's failure, not a quieter version of success: with no
            // cache there is nothing to publish but four empty lists.
            let mut captured = self.chain_identity().await;
            if captured.is_none() {
                // The re-read is unconditional, and the refresh's verdict is a log line rather than
                // a gate. `refresh_lists` returns `false` for two situations that have nothing in
                // common: it could not gather a set, or it *was overtaken* — a profile load, a
                // reconnect's refill or another poll's refresh published a complete set while this
                // one was reading, and the loser declined to overwrite it. In the second the cache
                // ends up filled, by the winner, which is the whole point of yielding to it. Reading
                // the cache only on `true` would throw that winner away, spend the retry, and report
                // "no coherent set" about a daemon whose lists were sitting complete in the cache.
                if !self.refresh_lists().await {
                    tracing::debug!(
                        "HQPlayer's list refresh published nothing; reading the cache anyway, \
                         since a refresh that lost a race leaves the winner's set in place"
                    );
                }
                captured = self.chain_identity().await;
            }
            let Some(captured) = captured else {
                if !retried {
                    tracing::info!(
                        "HQPlayer's setting lists did not settle; re-reading once before giving up"
                    );
                    retried = true;
                    continue;
                }
                return Err(anyhow!(
                    "HQPlayer's setting lists could not be read as one coherent set; refusing to \
                     report a pipeline with no settings in it, which is not what this daemon offers"
                ));
            };

            // (3)
            let observed = self.get_state().await?;
            // Adaptive transport and metadata must be observed, never manufactured from
            // `HqpStatus::default()`. A failed Status reply therefore invalidates this complete
            // coherent reading just as an unsettled chain does.
            let observed_status = self.get_playback_status().await?;

            // (4) A probe that could not run is not a probe that passed. "The cache was not
            // invalidated, so the lists are still the ones the state was read against" is true about
            // the *cache* and answers nothing about the *daemon*: whether it still has that chain
            // loaded is exactly what the failed request was asked. An explicit `result="Error"` is
            // terminal in `send_command` — no retry, no disconnect, no invalidation — so the cache
            // survives looking entirely usable, and a view projected through it is indistinguishable
            // from a verified one. A `warn!` does not make it one.
            let probe = match self.get_rates().await {
                Ok(probe) => probe,
                Err(e) if !retried => {
                    tracing::info!(
                        "HQPlayer's chain check could not be run ({e}); re-reading rather than \
                         resolving its indices against lists nothing has confirmed they belong to"
                    );
                    retried = true;
                    continue;
                }
                Err(e) => {
                    return Err(anyhow!(
                        "HQPlayer's loaded chain could not be verified while its settings were \
                         being read, and again during the re-read ({e}); refusing to report \
                         settings whose indices were never checked against the lists they are \
                         resolved through"
                    ))
                }
            };

            // (5) The proof hands back the values it proved. Nothing below re-reads the cache.
            match self.verified_chain_snapshot(captured, &probe).await {
                Ok(snapshot) => {
                    let session = match self.coherent_session_identity(attempt_generation).await {
                        Some(session) => session,
                        None if !retried => {
                            let closing_generation = self.transport_generation().await;
                            let transport_poisoned = self
                                .transport_poisoned
                                .load(std::sync::atomic::Ordering::Acquire);
                            tracing::info!(
                                "HQPlayer's native session changed or became unavailable while its \
                                 pipeline was being read (opening generation {attempt_generation}, \
                                 closing generation {closing_generation}, poisoned \
                                 {transport_poisoned}); restarting the complete read"
                            );
                            retried = true;
                            self.connect_with_operation_held().await.context(
                                "HQPlayer replacement transport could not complete the coherent \
                                 handshake required before re-reading its setting lists",
                            )?;
                            continue;
                        }
                        None => {
                            return Err(anyhow!(
                            "HQPlayer's native session changed or became unavailable while its \
                             setting lists and pipeline state were being read, and again during \
                             the re-read; refusing to resolve one session's chain indices through \
                             another session's lists"
                            ));
                        }
                    };
                    break (observed, observed_status, snapshot, vol_range, session);
                }
                Err(reason) if !retried => {
                    tracing::info!(
                        "HQPlayer's settings did not hold still while they were being read \
                         ({reason}); re-reading rather than publishing a state and a set of lists \
                         that were never shown to belong together"
                    );
                    retried = true;
                }
                // The retry did not settle either. There is nothing coherent to publish and no third
                // attempt. Answering `Ok` here hands back empty or unrelated option lists and
                // selections that resolve to nothing, presented as this daemon's settings — a caller
                // cannot tell that from a daemon that genuinely offers no filters. The route has
                // always had an error arm; this is what it is for.
                Err(reason) => {
                    return Err(anyhow!(
                        "HQPlayer's settings could not be read as one coherent set — {reason}, and \
                         again during the re-read; refusing to report a loaded chain's indices \
                         resolved through setting lists that belong to another"
                    ))
                }
            }
        };

        // The proven snapshot, and nothing else. Re-reading `self.state` for lists here is what this shape
        // exists to prevent: it would discard the set the proof covered in favour of whatever the
        // cache holds at this later moment. `hqplayer_pipeline_projection_lint` fails the build if it
        // creeps back.
        let ChainSnapshot {
            modes,
            filters,
            shapers,
            rates,
        } = snapshot;

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

        let legacy = PipelineStatus {
            status: PipelineState {
                state: state_str.to_string(),
                // State.mode and State.active_mode are INDEX (0,1,2) - look up by ModesItem.index
                mode: get_mode_by_index(state.mode),
                // `State.active_mode`, resolved through the modes list. This is a **reporting choice
                // between two fields whose semantics are not both measured**, not a statement that one
                // is right: `Status.active_mode` is measured to echo the configured mode under
                // `[source]` (HQP-C-023), and what `State.active_mode` reports there has never been
                // measured by anyone (HQP-C-024). An earlier comment here called the `Status` field
                // "unreliable" and instructed always using this one, which asserted the unmeasured half
                // as fact; #332 owns settling it.
                //
                // Nothing that has to be *correct* depends on this field. The loaded chain — which
                // decides every enumeration below — is resolved from the enumerations the daemon
                // serves, never from either `active_mode`.
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
        };

        Ok(CoherentPipelineSnapshot {
            legacy,
            state,
            playback_status,
            chain: ChainSnapshot {
                modes,
                filters,
                shapers,
                rates,
            },
            volume_range: vol_range,
            info: session.info,
            host: session.host,
            instance_name: session.instance_name,
            producer_epoch: session.producer_epoch,
            observed_at: SystemTime::now(),
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
        let _operation_guard = self.operation_lock.lock().await;
        self.web_request("/config", "GET", None).await
    }

    /// Fetch available profiles from web UI
    pub async fn fetch_profiles(&self) -> Result<Vec<HqpProfile>> {
        let _operation_guard = self.operation_lock.lock().await;
        self.fetch_profiles_under_operation().await
    }

    async fn fetch_profiles_under_operation(&self) -> Result<Vec<HqpProfile>> {
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

    /// Clear every cached value a profile switch can replace or make unsafe to reuse.
    ///
    /// This also runs for post-dispatch ambiguity: once the POST left this process, a missing or 5xx
    /// response cannot prove the daemon did not apply it.
    async fn invalidate_profile_dependent_cache(&self) {
        let mut state = self.state.write().await;
        Self::clear_chain_cache(&mut state);
        state.last_state = None;
        state.volume_range = None;
        state.hidden_fields.clear();
        state.profiles.clear();
        state.config_title = None;
    }

    /// Reconcile one received profile-POST response through the single recovery contract.
    async fn reconcile_profile_post_response(
        &self,
        response: reqwest::Response,
        profile_value: &str,
        operation: &str,
    ) -> Result<()> {
        if response.status().is_server_error() {
            self.invalidate_profile_dependent_cache().await;
            return Err(anyhow!(
                "{operation} outcome is ambiguous: server returned {} after receiving the \
                 mutating request",
                response.status()
            ));
        }
        if response.status().is_client_error() {
            return Err(anyhow!("Profile load failed: {}", response.status()));
        }

        // A profile replaces the daemon's settings wholesale, so everything cached from before it
        // describes a configuration that no longer exists. Drop first: `refresh_lists` correctly
        // preserves an existing cache after a transient read failure, but that behavior would retain
        // exactly the pre-profile universe that has just become unsafe.
        self.invalidate_profile_dependent_cache().await;
        if !self.refresh_lists().await {
            return Err(anyhow!(
                "{operation} was accepted, but the native setting universe did not recover \
                 coherently; the outcome is ambiguous"
            ));
        }
        self.fetch_profiles_under_operation()
            .await
            .map_err(|error| {
                anyhow!(
                    "{operation} was accepted and native settings recovered, but the persistent \
                     form did not recover for readback; the outcome is ambiguous ({error})"
                )
            })?;
        Err(anyhow!(
            "{operation} was accepted and the server recovered, but the outcome is ambiguous: the \
             supported protocol exposes no authoritative active-profile readback, so whether \
             profile {profile_value:?} was applied is unknown"
        ))
    }

    /// Get cached profiles
    pub async fn get_cached_profiles(&self) -> Vec<HqpProfile> {
        self.state.read().await.profiles.clone()
    }

    /// Load a profile via web UI form submission.
    ///
    /// The current daemon surface exposes no authoritative active-profile readback, so a dispatched
    /// POST never returns success. After an accepted POST this method still waits for coherent
    /// native recovery and a fresh persistent form, then reports recovered-but-unverified ambiguity.
    /// #330 owns the transactional readback needed to make this operation provable.
    pub async fn load_profile(&self, profile_value: &str) -> Result<()> {
        let _operation_guard = self.operation_lock.lock().await;
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
                self.fetch_profiles_under_operation().await?;
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

        let response = match request.body(body.clone()).send().await {
            Ok(response) => response,
            Err(error) => {
                self.invalidate_profile_dependent_cache().await;
                return Err(anyhow!(
                    "HQPlayer profile POST outcome is ambiguous: the request may have been applied, \
                     but no usable response established its result ({error})"
                ));
            }
        };

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

                    let response = match request.body(body).send().await {
                        Ok(response) => response,
                        Err(error) => {
                            self.invalidate_profile_dependent_cache().await;
                            return Err(anyhow!(
                                "HQPlayer profile POST retry outcome is ambiguous: the request may \
                                 have been applied, but no usable response established its result \
                                 ({error})"
                            ));
                        }
                    };
                    return self
                        .reconcile_profile_post_response(
                            response,
                            profile_value,
                            "HQPlayer profile POST retry",
                        )
                        .await;
                }
            }
            return Err(anyhow!("Authentication failed"));
        }

        self.reconcile_profile_post_response(response, profile_value, "HQPlayer profile POST")
            .await
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

    async fn get_matrix_profiles_with_generation(&self) -> Result<(Vec<MatrixProfile>, u64)> {
        let xml = Self::build_request("MatrixListProfiles", &[]);
        let (response, generation) = self.send_command_with_generation(&xml).await?;
        Ok((
            Self::parse_items(&response, "MatrixProfile", |item| MatrixProfile {
                index: Self::parse_attr_u32(item, "index"),
                name: Self::parse_attr(item, "name").unwrap_or_default(),
            }),
            generation,
        ))
    }

    /// The matrix profile the daemon currently has selected.
    ///
    /// The authority is the observed **`State.matrix_profile`** field, not `MatrixGetProfile`: #341
    /// records no versioned evidence that the supported daemon implements that command consistently,
    /// and #347 says explicitly not to treat it as the current-profile authority until it does.
    ///
    /// The `index` is *derived* by resolving that name against the list the daemon is serving now — it
    /// is a position in a list, so it is only meaningful alongside the list it came from and is never
    /// stored. A name the fresh list does not contain yields `None` rather than a profile carrying
    /// some other entry's index: nothing in the list is selected, and reporting index 0 would name
    /// whatever sits first, which on the observed corpus is `Default`.
    pub async fn get_matrix_profile(&self) -> Result<Option<MatrixProfile>> {
        Ok(self.get_matrix_profiles_and_current().await?.1)
    }

    /// Return the matrix list and selected entry from one configured endpoint and native session.
    pub async fn get_matrix_profiles_and_current(
        &self,
    ) -> Result<(Vec<MatrixProfile>, Option<MatrixProfile>)> {
        let _operation_guard = self.operation_lock.lock().await;
        let (profiles, generation) = self.get_matrix_profiles_with_generation().await?;
        let current = self
            .get_state_on_transport(generation)
            .await?
            .matrix_profile;
        if current.is_empty() {
            // No selection. The empty field is the default identity and is not list position 0.
            return Ok((profiles, None));
        }
        let resolved = profiles.iter().find(|p| p.name == current).cloned();
        if resolved.is_none() {
            tracing::warn!(
                "HQPlayer reports matrix profile {current:?}, which its own MatrixListProfiles does \
                 not contain; reporting no selection rather than another profile's index"
            );
        }
        Ok((profiles, resolved))
    }

    /// Ask the daemon what `MatrixGetProfile` reports.
    ///
    /// **This is an observation, not the authority.** [`Self::get_matrix_profile`] reads
    /// `State.matrix_profile`, because #341 records no versioned evidence that the supported daemon
    /// implements `MatrixGetProfile` consistently and #347 forbids assuming it does. This method
    /// exists so the tier-1 read-only capture lane can *watch* what the command says and diff it
    /// against `State` — which is how that question gets settled by evidence instead of by assumption.
    ///
    /// No control path calls it, and
    /// `no_production_code_treats_matrix_get_profile_as_the_current_selection` keeps it that way.
    pub async fn read_matrix_get_profile(&self) -> Result<Option<MatrixProfile>> {
        let xml = Self::build_request("MatrixGetProfile", &[]);
        let response = self.send_command(&xml).await?;

        // Both `value` (the Node.js reference's reading) and `name` are accepted, because which one
        // the daemon uses is part of what this lane is capturing.
        let index = Self::parse_attr_u32(&response, "index");
        let name =
            Self::parse_attr(&response, "value").or_else(|| Self::parse_attr(&response, "name"));

        match name {
            Some(n) if !n.is_empty() => Ok(Some(MatrixProfile { index, name: n })),
            _ => Ok(None),
        }
    }

    /// Select a matrix profile by its position in the daemon's current list.
    ///
    /// **This is the compatibility boundary and nothing else.** The legacy HTTP routes carry a
    /// number, so the number is accepted here, resolved against the list the daemon is serving now,
    /// and immediately turned into the semantic name that goes on the wire. The number is never
    /// stored, never published as identity, and never reaches the daemon.
    pub async fn set_matrix_profile(&self, profile_index: u32) -> Result<SettingOutcome> {
        let _operation_guard = self.operation_lock.lock().await;
        let (profiles, generation) = self.get_matrix_profiles_with_generation().await?;
        let name = profiles
            .iter()
            .find(|p| p.index == profile_index)
            .map(|p| p.name.clone())
            .ok_or_else(|| {
                anyhow!(
                    "Matrix profile {} is not a position in the list this daemon is serving now",
                    profile_index
                )
            })?;
        self.set_matrix_profile_named_under_operation(&name, generation)
            .await
    }

    /// Select a matrix profile by name, verifying against the authoritative `State` field.
    ///
    /// `MatrixSetProfile` takes the name, and like every other setter its `result="OK"` is not proof
    /// of application (HQP-C-028) — so the write is confirmed by reading `State.matrix_profile` back.
    pub async fn set_matrix_profile_named(&self, name: &str) -> Result<SettingOutcome> {
        let _operation_guard = self.operation_lock.lock().await;
        let (state, generation) = self.get_state_with_generation().await?;
        if state.matrix_profile == name {
            return Ok(SettingOutcome::AlreadySet);
        }
        self.write_matrix_profile_on_transport(name, generation)
            .await
    }

    async fn set_matrix_profile_named_under_operation(
        &self,
        name: &str,
        generation: u64,
    ) -> Result<SettingOutcome> {
        if self
            .get_state_on_transport(generation)
            .await?
            .matrix_profile
            == name
        {
            return Ok(SettingOutcome::AlreadySet);
        }
        self.write_matrix_profile_on_transport(name, generation)
            .await
    }

    async fn write_matrix_profile_on_transport(
        &self,
        name: &str,
        generation: u64,
    ) -> Result<SettingOutcome> {
        let xml = Self::build_request("MatrixSetProfile", &[("value", name)]);
        self.write_setting_on_transport(generation, &xml, "matrix profile", name.to_string(), |s| {
            Some(s.matrix_profile.clone())
        })
        .await
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
        let has_metadata =
            status.title.is_some() || status.artist.is_some() || status.album.is_some();
        let now_playing = if !status.track_id.is_empty() || status.length > 0 || has_metadata {
            Some(BusNowPlaying {
                title: status.title.clone().unwrap_or_default(),
                artist: status.artist.clone().unwrap_or_default(),
                album: status.album.clone().unwrap_or_default(),
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
struct HqpManagedWorker {
    shutdown: CancellationToken,
    join: tokio::task::JoinHandle<()>,
    phase: SharedWorkerPhase,
}

pub struct HqpInstanceManager {
    instances: Arc<RwLock<HashMap<String, Arc<HqpAdapter>>>>,
    bus: SharedBus,
    running: Arc<AtomicBool>,
    workers: Arc<Mutex<HashMap<String, HqpManagedWorker>>>,
    /// Serializes lifecycle and runtime instance mutations as one manager transaction.
    lifecycle_lock: Arc<Mutex<()>>,
    /// Optional contract-free composition seam. Keeping the legacy constructor makes standalone
    /// adapter users and compatibility harnesses exercise their unchanged public behavior.
    native_sink: Option<Arc<dyn HqpNativeObservationSink>>,
}

impl HqpInstanceManager {
    /// Create a new instance manager
    pub fn new(bus: SharedBus) -> Self {
        Self::new_with_optional_native_sink(bus, None)
    }

    /// Construct a manager that reports managed HQPlayer observations to a composition-root sink.
    pub fn new_with_native_sink(
        bus: SharedBus,
        native_sink: Arc<dyn HqpNativeObservationSink>,
    ) -> Self {
        Self::new_with_optional_native_sink(bus, Some(native_sink))
    }

    fn new_with_optional_native_sink(
        bus: SharedBus,
        native_sink: Option<Arc<dyn HqpNativeObservationSink>>,
    ) -> Self {
        Self {
            instances: Arc::new(RwLock::new(HashMap::new())),
            bus,
            running: Arc::new(AtomicBool::new(false)),
            workers: Arc::new(Mutex::new(HashMap::new())),
            lifecycle_lock: Arc::new(Mutex::new(())),
            native_sink,
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
        // Prepare outside the map lock, then use the entry as the atomic winner. Two callers may
        // prepare candidates, but they can no longer return different adapters for the same name.
        let adapter = Arc::new(HqpAdapter::new(self.bus.clone()));
        adapter.set_instance_name(name.to_string()).await;

        let mut instances = self.instances.write().await;
        instances.entry(name.to_string()).or_insert(adapter).clone()
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
        let _lifecycle_guard = self.lifecycle_lock.lock().await;
        let adapter = self.get_or_create(&name).await;
        adapter
            .configure(host, port, web_port, username, password)
            .await;
        self.save_to_config().await;
        if self.running.load(Ordering::SeqCst) {
            self.start_worker(name, adapter.clone()).await;
        }
        adapter
    }

    /// Remove an instance by name
    pub async fn remove_instance(&self, name: &str) -> bool {
        let _lifecycle_guard = self.lifecycle_lock.lock().await;
        let mut instances = self.instances.write().await;
        let removed = instances.remove(name);
        drop(instances);
        let Some(adapter) = removed else {
            return false;
        };

        self.stop_worker(name).await;
        // A removed instance is unlike a temporary native outage: its producer identity is gone.
        // Join first so no child can report a same-run observation behind this retirement.
        if let Some(sink) = &self.native_sink {
            if let Err(error) = sink
                .instance_removed(name, adapter.producer_epoch().await)
                .await
            {
                tracing::warn!(%error, instance = %name, "HQPlayer native observation sink could not retire removed instance; rolling removal back");
                // Retirement and removal are one transaction. Reporting success after the adapter
                // disappeared but its retained producer survived would leave a document that no
                // worker can ever refresh or retire. Restore the same adapter (and its worker when
                // this manager is live) so a caller can retry the definitive removal safely.
                self.instances
                    .write()
                    .await
                    .insert(name.to_string(), adapter.clone());
                if self.running.load(Ordering::SeqCst) {
                    self.start_worker(name.to_string(), adapter).await;
                }
                return false;
            }
        }
        // Removal is definitive even if the manager was not running and therefore had no worker.
        adapter.disconnect().await;
        self.save_to_config().await;
        true
    }

    /// Check if any instance is configured
    pub async fn has_instances(&self) -> bool {
        let instances = self.instances.read().await;
        !instances.is_empty()
    }

    /// Whether at least one child has a native endpoint and can enter the managed lifecycle.
    async fn has_configured_instances(&self) -> bool {
        let adapters: Vec<Arc<HqpAdapter>> =
            self.instances.read().await.values().cloned().collect();
        for adapter in adapters {
            if adapter.is_configured().await {
                return true;
            }
        }
        false
    }

    /// Start exactly one child observer for a configured runtime instance.
    async fn start_worker(&self, name: String, adapter: Arc<HqpAdapter>) {
        if !adapter.is_configured().await {
            return;
        }

        // Build the supervisor before taking the worker-map lock. The future is inert until
        // spawned; keeping its await expressions outside the map-guard scope also makes the actual
        // invariant obvious to the AST lock lint. Retry debt belongs to the supervisor and is
        // therefore preserved across replaceable observer children.
        let shutdown = CancellationToken::new();
        let supervisor_shutdown = shutdown.clone();
        let phase = Arc::new(RwLock::new(HqpWorkerPhase::Recovering));
        let recovery = Arc::new(Mutex::new(RecoveryState::new(
            adapter.recovery_config().await,
        )));
        let supervisor_name = name.clone();
        let run_adapter = adapter.clone();
        let cleanup_adapter = adapter;
        let supervisor_phase = phase.clone();
        let native_worker = self.native_worker(&name);
        let supervisor = async move {
            supervise_worker(
                supervisor_name,
                supervisor_shutdown,
                supervisor_phase,
                recovery,
                move |child_shutdown, child_phase, child_recovery| {
                    let adapter = run_adapter.clone();
                    let native_worker = native_worker.clone();
                    async move {
                        adapter
                            .run_managed(child_shutdown, child_phase, child_recovery, native_worker)
                            .await;
                    }
                },
                move || {
                    let adapter = cleanup_adapter.clone();
                    async move {
                        adapter.disconnect().await;
                    }
                },
            )
            .await;
        };

        let mut workers = self.workers.lock().await;
        // Recheck while holding the worker-set lock. A concurrent stop may have flipped `running`
        // after `add_instance` decided this call was needed; spawning after stop drained the map
        // would otherwise create an orphan observer.
        if !self.running.load(Ordering::SeqCst) {
            return;
        }
        if workers.contains_key(&name) {
            return;
        }

        let join = tokio::spawn(supervisor);
        workers.insert(
            name,
            HqpManagedWorker {
                shutdown,
                join,
                phase,
            },
        );
    }

    fn native_worker(&self, instance_name: &str) -> Option<HqpNativeWorker> {
        Some(HqpNativeWorker {
            sink: self.native_sink.clone()?,
            instance_name: instance_name.to_string(),
        })
    }

    /// Cancel and join one child before returning so it cannot republish after removal.
    async fn stop_worker(&self, name: &str) {
        let worker = self.workers.lock().await.remove(name);
        if let Some(worker) = worker {
            worker.shutdown.cancel();
            if let Err(error) = worker.join.await {
                tracing::warn!("HQPlayer child lifecycle failed to join: {error}");
            }
        }
    }

    async fn start_internal(&self) -> Result<()> {
        let _lifecycle_guard = self.lifecycle_lock.lock().await;
        if self.running.load(Ordering::SeqCst) {
            return Ok(());
        }

        let candidates: Vec<(String, Arc<HqpAdapter>)> = self
            .instances
            .read()
            .await
            .iter()
            .map(|(name, adapter)| (name.clone(), adapter.clone()))
            .collect();
        let mut adapters = Vec::new();
        for (name, adapter) in candidates {
            if adapter.is_configured().await {
                adapters.push((name, adapter));
            }
        }

        if adapters.is_empty() {
            return Err(anyhow!("No configured HQPlayer instances"));
        }

        if let Some(sink) = &self.native_sink {
            sink.manager_started().await?;
        }
        self.running.store(true, Ordering::SeqCst);
        for (name, adapter) in adapters {
            self.start_worker(name, adapter).await;
        }

        Ok(())
    }

    async fn stop_internal(&self) {
        let _lifecycle_guard = self.lifecycle_lock.lock().await;
        if !self.running.swap(false, Ordering::SeqCst) {
            return;
        }
        self.bus.publish(BusEvent::AdapterStopping {
            adapter: "hqplayer".to_string(),
            reason: Some("HQPlayer lifecycle stopped".to_string()),
        });

        let workers = {
            let mut workers = self.workers.lock().await;
            std::mem::take(&mut *workers)
        };
        for worker in workers.values() {
            worker.shutdown.cancel();
        }
        for (_, worker) in workers {
            if let Err(error) = worker.join.await {
                tracing::warn!("HQPlayer child lifecycle failed to join: {error}");
            }
        }
        if let Some(sink) = &self.native_sink {
            if let Err(error) = sink.manager_stopped().await {
                tracing::warn!(%error, "HQPlayer native observation sink could not finish manager lifecycle");
            }
        }
        self.bus.publish(BusEvent::AdapterStopped {
            adapter: "hqplayer".to_string(),
        });
    }

    /// Get instance count
    pub async fn instance_count(&self) -> usize {
        let instances = self.instances.read().await;
        instances.len()
    }

    /// Internal supervisor/child status without changing any serialized status payload.
    pub async fn worker_status(&self, name: &str) -> HqpWorkerStatus {
        let phase = self
            .workers
            .lock()
            .await
            .get(name)
            .map(|worker| worker.phase.clone());
        let supervisor_enabled = self.running.load(Ordering::SeqCst) && phase.is_some();
        let phase = match phase {
            Some(phase) => *phase.read().await,
            None => HqpWorkerPhase::Stopped,
        };
        HqpWorkerStatus {
            supervisor_enabled,
            phase,
        }
    }
}

#[async_trait::async_trait]
impl Startable for HqpInstanceManager {
    fn name(&self) -> &'static str {
        "hqplayer"
    }

    async fn start(&self) -> Result<()> {
        self.start_internal().await
    }

    async fn stop(&self) {
        self.stop_internal().await;
    }

    async fn can_start(&self) -> bool {
        self.has_configured_instances().await
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

#[cfg(test)]
mod parse_attr_scope_tests {
    use super::*;

    /// Root-scoping, asserted directly on `parse_attr` (CodeRabbit thread 11).
    ///
    /// The conformance suite already covers this end to end, but that test accepts *any* error from
    /// the adapter, so framing can reject the orphaned buffer before scoping is ever consulted and the
    /// test stays green even with whole-buffer scanning restored. It therefore pins the outcome
    /// without pinning the mechanism. These cases call the function itself, where nothing else can
    /// satisfy them.
    #[test]
    fn an_attribute_outside_any_root_element_is_not_reported() {
        // A bare fragment: the attribute is present in the text but no root element opens, so there is
        // no scope that could legitimately own it.
        let rootless = " state=\"2\" volume=\"-1\"/>";
        assert_eq!(HqpAdapter::parse_attr(rootless, "volume"), None);
        assert_eq!(HqpAdapter::parse_attr(rootless, "state"), None);
    }

    #[test]
    fn a_leading_orphan_can_never_supply_an_attribute() {
        // The corruption shape: an orphaned fragment carrying a conflicting `volume` precedes a
        // well-formed reply. `root_open_tag` refuses a buffer that does not *start* at a root element,
        // so the answer is `None` rather than the later document's value — the buffer is not one
        // document and the function does not guess which part to trust.
        //
        // This is the discriminating case. Whole-buffer scanning returns `Some("-1")` here, taking the
        // orphan's value because it appears first; that is the silent corruption this scoping removed.
        // Production never presents this shape — `send_command_inner` hands over exactly one document —
        // which is why it has to be asserted here rather than through the adapter.
        let orphan_then_reply = concat!(
            " state=\"2\" volume=\"-1\"/>\n",
            "<State state=\"1\" volume=\"-23.5\" mode=\"1\"/>"
        );
        assert_eq!(
            HqpAdapter::parse_attr(orphan_then_reply, "volume"),
            None,
            "a buffer that does not begin at a root element has no attribute to report"
        );
    }

    #[test]
    fn an_attribute_of_the_root_element_is_still_reported_normally() {
        // Guard against over-tightening: the ordinary case must keep working.
        let reply = "<?xml version=\"1.0\" encoding=\"UTF-8\"?><State volume=\"-12.5\"/>";
        assert_eq!(
            HqpAdapter::parse_attr(reply, "volume").as_deref(),
            Some("-12.5")
        );
    }

    /// A `>` inside a **single-quoted** attribute value must not end the root tag's scope
    /// (CodeRabbit review 4816484338).
    ///
    /// XML permits either quote form, and `root_frame_end` already tracks both — deliberately, because
    /// a single bool for `"` alone lets `song='</Status>'` end a frame. `root_open_tag` tracked only
    /// `"`, so the scope stopped at the `>` *inside* the value and every attribute after it read as
    /// absent. That is silent: `parse_attr` returns `None`, and callers substitute `0`/`""` rather
    /// than reporting a parse failure, so a playing track whose title contains `>` would zero the
    /// volume the UI shows.
    ///
    /// Asserted on `parse_attr` and on `root_open_tag` both: the outcome is what callers depend on,
    /// but only the scope assertion pins the *mechanism*, and an outcome test can pass for the wrong
    /// reason.
    #[test]
    fn a_gt_inside_a_single_quoted_value_does_not_truncate_the_root_scope() {
        // The daemon sends `song` entity-escaped, but the reference warns a bare character has been
        // observed too, so this is the shape that must not corrupt the read.
        let reply = "<State song='a>b' state=\"1\" volume=\"-23.5\"/>";

        assert_eq!(
            framing::root_open_tag(reply),
            Some("<State song='a>b' state=\"1\" volume=\"-23.5\"/>"),
            "the root scope must reach the tag's own `>`, not the one inside a single-quoted value"
        );
        assert_eq!(
            HqpAdapter::parse_attr(reply, "volume").as_deref(),
            Some("-23.5"),
            "an attribute after a single-quoted value containing `>` must still be found; `None` \
             here becomes a silent 0 dB in every caller"
        );
        assert_eq!(HqpAdapter::parse_attr(reply, "state").as_deref(), Some("1"));
    }

    /// The negative side of the same rule: a `'` inside a **double-quoted** value must not open a
    /// quote region, and an apostrophe in ordinary text is exactly where that would bite.
    #[test]
    fn an_apostrophe_inside_a_double_quoted_value_does_not_open_a_quote_region() {
        let reply = "<State song=\"Gloria's Step\" volume=\"-11.5\"/>";
        assert_eq!(
            framing::root_open_tag(reply),
            Some("<State song=\"Gloria's Step\" volume=\"-11.5\"/>"),
            "only the character that opened a value may close it; a lone `'` inside `\"…\"` is content"
        );
        assert_eq!(
            HqpAdapter::parse_attr(reply, "volume").as_deref(),
            Some("-11.5")
        );
    }

    /// An **unterminated** single quote must not be rescued by guessing a boundary.
    ///
    /// With the quote still open there is no way to tell markup from data, so the honest answer is no
    /// scope at all — the same shape `root_frame_end` deliberately declines to resolve.
    #[test]
    fn an_unterminated_single_quote_yields_no_root_scope() {
        let truncated = "<State song='a>b state=\"1\"";
        assert_eq!(framing::root_open_tag(truncated), None);
        assert_eq!(HqpAdapter::parse_attr(truncated, "state"), None);
    }
}

/// The identity-and-snapshot rule the pipeline read rests on (issue #347, Opus dissent at `b7a7a1a`).
///
/// These reach past the adapter's public surface deliberately. The rule is about **which cache** a
/// verified state is joined to, and the window it closes is between two moments inside one function:
/// no sequence of public calls can place a second actor in that window without a production hook, and
/// a test-only hook would be testing the hook. What *can* be pinned honestly is the primitive — that
/// the proof answers about the cache it hands back, that four fingerprints and not one decide whether
/// a set is the same set, and that only the daemon contradicting the cache is grounds for dropping it.
///
/// The structural half of the same rule — that the projection never re-reads what the proof returned —
/// is `tests/hqplayer_pipeline_projection_lint.rs`, which is red at `b7a7a1a`.
#[cfg(test)]
// The crate denies `expect` because a panic in production is not an error report. In a test a panic
// *is* the report, and an `expect` whose message states the precondition it is asserting is clearer
// than a `match` that fabricates a fallback the test would then silently pass against.
#[allow(clippy::expect_used)]
mod chain_identity_tests {
    use super::*;

    fn mode(index: u32, name: &str) -> ListItem {
        ListItem {
            index,
            name: name.to_string(),
            value: 0,
        }
    }

    fn filter(index: u32, name: &str) -> FilterItem {
        FilterItem {
            index,
            name: name.to_string(),
            value: 0,
            arg: 0,
        }
    }

    fn rate(index: u32, rate: u32) -> RateItem {
        RateItem { index, rate }
    }

    /// A complete, self-consistent cache: every family filled and fingerprinted from its contents,
    /// which is the invariant every writer in the adapter maintains.
    fn cache_of(
        modes: Vec<ListItem>,
        filters: Vec<FilterItem>,
        shapers: Vec<ListItem>,
        rates: Vec<RateItem>,
    ) -> HqpAdapterState {
        HqpAdapterState {
            modes_fingerprint: Some(HqpAdapter::fingerprint(
                modes.iter().map(|m| (m.index, m.name.as_str())),
            )),
            filters_fingerprint: Some(HqpAdapter::fingerprint(
                filters.iter().map(|f| (f.index, f.name.as_str())),
            )),
            shapers_fingerprint: Some(HqpAdapter::fingerprint(
                shapers.iter().map(|s| (s.index, s.name.as_str())),
            )),
            rates_fingerprint: Some(HqpAdapter::rates_fingerprint(&rates)),
            modes,
            filters,
            shapers,
            rates,
            ..HqpAdapterState::default()
        }
    }

    fn pcm_cache() -> HqpAdapterState {
        cache_of(
            vec![mode(0, "[source]"), mode(1, "PCM")],
            vec![filter(0, "sinc-M"), filter(1, "poly-sinc-xtr")],
            vec![mode(0, "NS9"), mode(1, "NS5")],
            vec![rate(0, 44100), rate(1, 48000)],
        )
    }

    /// **The rule the rate list alone cannot express.** A profile load can replace the filters and the
    /// shapers while leaving the rate list byte-identical — that is not hypothetical, it is what
    /// `a_profile_load_whose_refresh_fails_does_not_leave_the_previous_profiles_lists_in_place`
    /// exercises end to end — and a read that captured only the rates would accept the replacement as
    /// the set it started from and join its `State`'s filter index to another profile's filter list.
    #[test]
    fn a_replaced_filter_list_is_a_different_set_even_when_the_rates_did_not_move() {
        let before = pcm_cache();
        let after = cache_of(
            before.modes.clone(),
            // A different profile's filters, at the same positions.
            vec![filter(0, "closed-form"), filter(1, "gauss-long")],
            before.shapers.clone(),
            before.rates.clone(),
        );

        let before_id =
            HqpAdapter::chain_identity_of(&before).expect("a complete set has identity");
        let after_id = HqpAdapter::chain_identity_of(&after).expect("a complete set has identity");

        assert_eq!(
            before_id.rates, after_id.rates,
            "precondition: the rate list is untouched, so a rates-only identity sees no change at all"
        );
        assert_ne!(
            before_id, after_id,
            "but the set is not the same set, and the identity a read captures has to say so"
        );
    }

    /// The same rule for the other two families, so the identity cannot be narrowed to "rates and
    /// filters" either.
    #[test]
    fn a_replaced_shaper_or_mode_list_is_also_a_different_set() {
        let before = pcm_cache();
        let shapers_moved = cache_of(
            before.modes.clone(),
            before.filters.clone(),
            vec![mode(0, "ASDM7"), mode(1, "ASDM5")],
            before.rates.clone(),
        );
        let modes_moved = cache_of(
            vec![mode(0, "[source]"), mode(1, "SDM")],
            before.filters.clone(),
            before.shapers.clone(),
            before.rates.clone(),
        );

        let before_id = HqpAdapter::chain_identity_of(&before).expect("identity");
        assert_ne!(
            before_id,
            HqpAdapter::chain_identity_of(&shapers_moved).expect("identity"),
            "the shaper list decides what a shaper index names"
        );
        assert_ne!(
            before_id,
            HqpAdapter::chain_identity_of(&modes_moved).expect("identity"),
            "and the mode list decides what a mode index names"
        );
    }

    /// Presence is not a separate question asked separately — that separation is how an emptied cache
    /// passed for an unchanged one. A set that is missing a family has no identity to compare, so the
    /// proof cannot pass it whatever else is true.
    #[test]
    fn a_cache_missing_any_family_has_no_identity() {
        for drop_one in 0..4 {
            let mut state = pcm_cache();
            match drop_one {
                0 => state.modes.clear(),
                1 => state.filters.clear(),
                2 => state.shapers.clear(),
                _ => state.rates.clear(),
            }
            assert!(
                HqpAdapter::chain_identity_of(&state).is_none(),
                "family {drop_one} is empty, so this is not a set"
            );
        }
    }

    /// A family present without the fingerprint it was filled from is unanchored: there is nothing to
    /// compare it against, so it cannot take part in an identity. Current writers keep the two in
    /// lockstep, which makes this defensive rather than presently reachable — and it is the invariant
    /// that keeps it that way.
    #[test]
    fn a_family_without_the_fingerprint_it_was_filled_from_has_no_identity() {
        for drop_one in 0..4 {
            let mut state = pcm_cache();
            match drop_one {
                0 => state.modes_fingerprint = None,
                1 => state.filters_fingerprint = None,
                2 => state.shapers_fingerprint = None,
                _ => state.rates_fingerprint = None,
            }
            assert!(
                HqpAdapter::chain_identity_of(&state).is_none(),
                "fingerprint {drop_one} is missing, so that family is unanchored"
            );
        }
    }

    /// A generation token only has to distinguish cache events that can overlap one in-flight
    /// gather/read. Saturating at `u64::MAX` would make the very next clear or publication invisible
    /// forever: `MAX -> MAX`. Wrapping keeps every adjacent event distinct; an ABA collision would
    /// require 2^64 cache mutations during that one bounded operation.
    #[test]
    fn the_generation_counter_never_stalls_at_the_integer_ceiling() {
        let mut state = HqpAdapterState {
            chain_generation: u64::MAX,
            ..HqpAdapterState::default()
        };

        HqpAdapter::bump_chain_generation(&mut state);
        assert_eq!(
            state.chain_generation, 0,
            "the event after MAX must still produce a distinct token; saturation would make it \
             invisible to every in-flight publication"
        );
        HqpAdapter::bump_chain_generation(&mut state);
        assert_eq!(
            state.chain_generation, 1,
            "and the counter must continue advancing after the wrap"
        );
    }

    /// The four families of a cache, as the value a gather hands to the publisher.
    fn gathered(cache: &HqpAdapterState) -> ChainSnapshot {
        ChainSnapshot {
            modes: cache.modes.clone(),
            filters: cache.filters.clone(),
            shapers: cache.shapers.clone(),
            rates: cache.rates.clone(),
        }
    }

    /// An adapter whose cache is a given complete set, and nothing else. No socket, no config: these
    /// tests are about the proof, and the proof is a decision about memory.
    async fn adapter_with_cache(cache: HqpAdapterState) -> HqpAdapter {
        let adapter = HqpAdapter::new(crate::bus::create_bus());
        let mut state = adapter.state.write().await;
        state.modes = cache.modes;
        state.filters = cache.filters;
        state.shapers = cache.shapers;
        state.rates = cache.rates;
        state.modes_fingerprint = cache.modes_fingerprint;
        state.filters_fingerprint = cache.filters_fingerprint;
        state.shapers_fingerprint = cache.shapers_fingerprint;
        state.rates_fingerprint = cache.rates_fingerprint;
        drop(state);
        adapter
    }

    /// The proof's whole point: it returns **the set it verified**, so what a caller publishes and
    /// what was proved coherent are the same values rather than two reads of the same cache.
    #[tokio::test]
    async fn the_proof_hands_back_the_exact_set_it_verified() {
        let cache = pcm_cache();
        let expected = cache.filters.clone();
        let probe = cache.rates.clone();
        let adapter = adapter_with_cache(cache).await;
        let captured = adapter.chain_identity().await.expect("a complete cache");

        let snapshot = adapter
            .verified_chain_snapshot(captured, &probe)
            .await
            .expect("an unchanged cache and an agreeing probe are a verified read");

        assert_eq!(
            snapshot
                .filters
                .iter()
                .map(|f| f.name.as_str())
                .collect::<Vec<_>>(),
            expected.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
            "the returned snapshot is what the caller projects; it must be the verified contents"
        );
        assert_eq!(snapshot.modes.len(), 2);
        assert_eq!(snapshot.shapers.len(), 2);
        assert_eq!(snapshot.rates.len(), 2);
    }

    /// **The race the dissent names.** A complete, coherent, *different* cache is in place by the time
    /// the proof runs — a profile load, a reconnect's refill or another poll's refresh got there
    /// first. Presence says yes; the rate probe says yes, because this replacement did not touch the
    /// rates. Only the captured identity can refuse it, and refuse it it must: the `State` this read
    /// holds was reported against the *other* set.
    ///
    /// And the new cache stays. It is not known to be wrong — it is merely newer than this read, and
    /// dropping it would make one read's bad luck into everyone's refetch.
    #[tokio::test]
    async fn a_cache_replaced_mid_read_fails_this_read_and_is_left_in_place() {
        let before = pcm_cache();
        let probe = before.rates.clone();
        let adapter = adapter_with_cache(before).await;
        let captured = adapter.chain_identity().await.expect("a complete cache");

        // The replacement: a complete set from another profile, same rate list.
        let replacement = cache_of(
            vec![mode(0, "[source]"), mode(1, "PCM")],
            vec![filter(0, "closed-form"), filter(1, "gauss-long")],
            vec![mode(0, "NS9"), mode(1, "NS5")],
            probe.clone(),
        );
        {
            let mut state = adapter.state.write().await;
            state.filters = replacement.filters.clone();
            state.filters_fingerprint = replacement.filters_fingerprint;
        }

        let outcome = adapter.verified_chain_snapshot(captured, &probe).await;

        assert_eq!(
            outcome.err(),
            Some(ChainProofFailure::Replaced),
            "a complete replacement is present and unmoved, and it is still not the set this read \
             captured; publishing its lists against the captured state's indices is the \
             misselection this issue exists to end"
        );
        let state = adapter.state.read().await;
        assert_eq!(
            state
                .filters
                .iter()
                .map(|f| f.name.as_str())
                .collect::<Vec<_>>(),
            vec!["closed-form", "gauss-long"],
            "and the newer set stands: this read failed, the cache did not"
        );
    }

    /// The probe is an observation taken **before** the proof acquires the cache lock. If another
    /// reader publishes a complete newer set after that observation, the probe can disagree with
    /// the cache simply because it describes the set this read started from. That is `Replaced`, not
    /// evidence that the newer cache is stale, even when the replacement also has different rates.
    ///
    /// Classification order is therefore correctness, not presentation: compare the current
    /// four-family identity with the captured identity first. Only when they are the same set may a
    /// disagreeing live probe invalidate it.
    #[tokio::test]
    async fn an_old_probe_never_clears_a_newer_complete_cache_with_different_rates() {
        let before = pcm_cache();
        let old_probe = before.rates.clone();
        let adapter = adapter_with_cache(before).await;
        let captured = adapter.chain_identity().await.expect("a complete cache");

        let replacement = cache_of(
            vec![mode(0, "[source]"), mode(2, "SDM")],
            vec![filter(0, "poly-sinc-gauss-hires-lp"), filter(1, "sinc-L")],
            vec![mode(0, "ASDM7"), mode(1, "ASDM5")],
            vec![rate(0, 2_822_400), rate(1, 5_644_800)],
        );
        let replacement_id =
            HqpAdapter::chain_identity_of(&replacement).expect("the replacement is complete");
        {
            let mut state = adapter.state.write().await;
            state.modes = replacement.modes;
            state.filters = replacement.filters;
            state.shapers = replacement.shapers;
            state.rates = replacement.rates;
            state.modes_fingerprint = replacement.modes_fingerprint;
            state.filters_fingerprint = replacement.filters_fingerprint;
            state.shapers_fingerprint = replacement.shapers_fingerprint;
            state.rates_fingerprint = replacement.rates_fingerprint;
        }

        let outcome = adapter.verified_chain_snapshot(captured, &old_probe).await;

        assert_eq!(
            outcome.err(),
            Some(ChainProofFailure::Replaced),
            "the old probe and newer cache belong to different read generations; this read must be \
             retried without accusing the newer set of contradicting the daemon"
        );
        assert_eq!(
            adapter.chain_identity().await,
            Some(replacement_id),
            "a probe taken before the replacement is not grounds to erase the complete cache that \
             replaced it"
        );
    }

    /// The one failure that *is* grounds for dropping the cache: the daemon's live rate list
    /// contradicts the rates currently cached. That is the daemon disagreeing with the cache, so the
    /// cache is wrong — and the next read must refill rather than compare against it again.
    #[tokio::test]
    async fn a_probe_that_contradicts_the_cached_rates_drops_the_cache() {
        let cache = pcm_cache();
        let adapter = adapter_with_cache(cache).await;
        let captured = adapter.chain_identity().await.expect("a complete cache");
        // The other chain's rates (HQP-C-020: the two were observed disjoint).
        let probe = vec![rate(0, 2822400), rate(1, 5644800)];

        let outcome = adapter.verified_chain_snapshot(captured, &probe).await;

        assert_eq!(
            outcome.err(),
            Some(ChainProofFailure::Moved),
            "the daemon is serving a rate list these lists were not read from"
        );
        assert!(
            adapter.chain_identity().await.is_none(),
            "a cache the daemon contradicts must not survive to be compared against again"
        );
    }

    /// The mirror of the case above, and the one that keeps the rule from degrading into "any
    /// difference drops the cache": an unchanged set with an agreeing probe leaves everything alone.
    #[tokio::test]
    async fn a_verified_read_changes_nothing() {
        let cache = pcm_cache();
        let probe = cache.rates.clone();
        let adapter = adapter_with_cache(cache).await;
        let captured = adapter.chain_identity().await.expect("a complete cache");

        adapter
            .verified_chain_snapshot(captured, &probe)
            .await
            .expect("verified");

        assert_eq!(
            adapter.chain_identity().await,
            Some(captured),
            "proving a read must not disturb the cache it proved"
        );
    }

    /// **The write-side twin of `a_cache_replaced_mid_read_fails_this_read_and_is_left_in_place`,
    /// and the one the read-side guard cannot cover.** `refresh_lists` gathers five replies off the
    /// state lock and only then takes it. Everything the read path does to avoid overwriting a
    /// winner happens on the *read* side; the refresh writer, having taken no position on what it
    /// was replacing, used to assign all four families unconditionally.
    ///
    /// So: a profile load lands while the gather is in flight, invalidating the cache and refilling
    /// it from the new profile. The gather's closing `GetRates` then returns — the rate list is
    /// unchanged across the two profiles (HQP-C-021 makes that ordinary), so the bracket agrees with
    /// itself and the old writer publishes the *previous* profile's filters and shapers over the
    /// new profile's. Nothing downstream can detect it: a later read captures that cache, probes,
    /// finds the rates agreeing, and reports the old profile's filter name for a `State` produced by
    /// the new one. Confidently, which is the part that matters.
    ///
    /// Only the generation can refuse this. The four fingerprints cannot: the winner is complete,
    /// self-consistent and — on the rates — identical.
    ///
    /// **Label: client-red.** Found by CodeRabbit at `9b1fdfe`.
    #[tokio::test]
    async fn a_gather_overtaken_by_a_newer_profile_publishes_nothing_and_leaves_the_winner() {
        let old_profile = pcm_cache();
        let unchanged_rates = old_profile.rates.clone();
        let old_modes = old_profile.modes.clone();
        // What the five replies will amount to, gathered off the state lock.
        let stale_gather = gathered(&old_profile);
        let adapter = adapter_with_cache(old_profile).await;

        // The gather begins: this is the cache its replies were asked on behalf of.
        let since = adapter.chain_generation().await;

        // Mid-gather: a profile load drops the cache and republishes it from the new profile. Same
        // rates — a profile can move the filters and shapers while leaving the rate list alone.
        adapter.invalidate_chain_cache().await;
        let winner = cache_of(
            old_modes,
            vec![filter(0, "closed-form"), filter(1, "gauss-long")],
            vec![mode(0, "ASDM7"), mode(1, "ASDM5")],
            unchanged_rates,
        );
        assert!(
            adapter
                .publish_chain_snapshot(gathered(&winner), adapter.chain_generation().await)
                .await,
            "precondition: the newer profile's set publishes, since nothing overtook it"
        );
        let winner_id = adapter
            .chain_identity()
            .await
            .expect("the winner is the cache now");

        // And only now does the old gather reach the lock.
        let published = adapter.publish_chain_snapshot(stale_gather, since).await;

        assert!(
            !published,
            "the cache this gather set out to fill has been filled by someone newer; publishing \
             over it would replace a current set with one read before the profile changed"
        );
        assert_eq!(
            adapter.chain_identity().await,
            Some(winner_id),
            "and the winner is left exactly as it was — an overtaken writer reports its loss to its \
             caller, it does not take the cache down with it"
        );
        assert_eq!(
            adapter
                .state
                .read()
                .await
                .filters
                .iter()
                .map(|f| f.name.as_str())
                .collect::<Vec<_>>(),
            vec!["closed-form", "gauss-long"],
            "which is to say: the new profile's filters, not the ones this gather was holding"
        );
    }

    /// The other half of the same rule, and the half a refill-shaped test cannot reach: an
    /// invalidation with **nothing** published after it must stop an overtaken gather too.
    ///
    /// That is the reconnect, not the profile load. `connect`, `disconnect` and `mark_disconnected`
    /// all drop the cache and leave it empty, and a gather straddling one of them read some of its
    /// five replies through a session that has since ended — through a daemon that may have
    /// restarted, been reconfigured, or followed the source onto the other chain while UHC was away.
    /// Publishing that set fills an emptied cache with exactly the lists the emptying was for.
    ///
    /// Written because the mutation check found it: deleting the bump from `clear_chain_cache` left
    /// the two races above still passing, since in both of them the *refill's* bump does the work.
    /// Only a clearing with no refill isolates the clearing's own bump.
    #[tokio::test]
    async fn an_invalidation_with_no_refill_stops_an_overtaken_gather_as_well() {
        let cache = pcm_cache();
        let stale_gather = gathered(&cache);
        let adapter = adapter_with_cache(cache).await;
        let since = adapter.chain_generation().await;

        // Mid-gather the session dropped. Nothing refills it — a reconnect leaves the cache empty
        // and the next read fills it.
        adapter.invalidate_chain_cache().await;

        let published = adapter.publish_chain_snapshot(stale_gather, since).await;

        assert!(
            !published,
            "these lists were read through a session that has ended; an empty cache is not an \
             invitation to fill it with them"
        );
        assert!(
            adapter.chain_identity().await.is_none(),
            "and the cache stays empty, so the next read refills it from the daemon it is actually \
             talking to"
        );
    }

    /// The generation is part of the **identity**, not just a publication guard, and this is why.
    ///
    /// A profile load, a reconnect or a mode write clears the cache and refills it. That refill can
    /// come back byte-identical in all four families — the same daemon reloading the same
    /// configuration is the ordinary case, not a contrived one — while `State` is emphatically not
    /// the same: the session, the profile and the loaded chain behind it all changed, which is the
    /// event that made the cache be dropped in the first place.
    ///
    /// A read that captured the pre-clear cache and then observed `State` therefore holds a `State`
    /// from after the event and an identity from before it. On fingerprints alone those two caches
    /// compare equal and the read passes, joining that `State`'s indices to lists it never saw
    /// filled. The generation is the only thing in the identity that separates "untouched" from
    /// "torn down and rebuilt to look the same".
    #[tokio::test]
    async fn a_byte_identical_refill_is_still_a_replacement_for_a_read_that_captured_the_old_cache()
    {
        let cache = pcm_cache();
        let probe = cache.rates.clone();
        let refill = gathered(&cache);
        let adapter = adapter_with_cache(cache).await;
        let captured = adapter.chain_identity().await.expect("a complete cache");

        // Torn down and rebuilt from the same four lists.
        adapter.invalidate_chain_cache().await;
        assert!(
            adapter
                .publish_chain_snapshot(refill, adapter.chain_generation().await)
                .await,
            "precondition: the refill publishes"
        );

        let outcome = adapter.verified_chain_snapshot(captured, &probe).await;

        assert_eq!(
            outcome.err(),
            Some(ChainProofFailure::Replaced),
            "the lists read the same, and they are still not the filling this read captured; the \
             `State` it holds was reported after the event that emptied that one"
        );
        assert!(
            adapter.chain_identity().await.is_some(),
            "the refill is the current cache and is not known to be wrong; this read retries, the \
             cache stands"
        );
    }

    /// The four families of a cache, each as the value a **single-family** fetch would hand the
    /// publisher — which is what a setter's `fresh_*` does, as opposed to `refresh_lists`' whole set.
    fn each_family(cache: &HqpAdapterState) -> Vec<FreshFamily> {
        vec![
            FreshFamily::Modes(cache.modes.clone()),
            FreshFamily::Filters(cache.filters.clone()),
            FreshFamily::Shapers(cache.shapers.clone()),
            FreshFamily::Rates(cache.rates.clone()),
        ]
    }

    /// **The single-family twin of
    /// `a_gather_overtaken_by_a_newer_profile_publishes_nothing_and_leaves_the_winner`, and the one
    /// with a wire consequence.** `refresh_lists` publishes a whole set and returns a `bool`;
    /// `fresh_filters` publishes *one* family and hands the list **back to a setter**, which resolves
    /// a filter name to a position in it and sends that position to the daemon.
    ///
    /// So: a setter's `GetFilters` goes out against the loaded profile. While the reply is in flight
    /// a profile load clears the cache and refills it completely from the new profile — same rate
    /// list, which is ordinary, since a profile can move the filters and shapers and leave the
    /// offered rates alone (HQP-C-021). The reply then arrives holding the *previous* profile's
    /// filters.
    ///
    /// Two things must not happen, and both did. The cache must not be disturbed: the winner is
    /// complete, current, and not known to be wrong. And the list must not be returned to the setter,
    /// because the index it resolves a name to is a position in a list the daemon is no longer
    /// serving — HQP-C-001's misselection, arriving through the write path.
    ///
    /// The damage the old shape could do had two shapes, depending on where the profile load landed
    /// inside the publisher. Landing before the previous-fingerprint read, it left a *partial* cache:
    /// the fingerprints disagreed, so every family was dropped and only this one refilled. Landing
    /// *between* that read and the write — two separate lock acquisitions, which is what made the
    /// window — it left a **complete mixed** cache: three families from the new profile and one from
    /// the old, with nothing missing for a later read to notice and a rate probe that agrees. One
    /// guard closes both, and closes the second by construction: the comparison and the write happen
    /// under one lock, and the generation captured before the request decides whether either runs.
    ///
    /// **Label: client-red.**
    #[tokio::test]
    async fn a_stale_filter_fetch_overtaken_by_a_complete_winner_is_refused_and_leaves_it_alone() {
        let old_profile = pcm_cache();
        let unchanged_rates = old_profile.rates.clone();
        let old_modes = old_profile.modes.clone();
        // What `GetFilters` returned, off the state lock: the profile that was loaded at the time.
        let stale_filters = FreshFamily::Filters(old_profile.filters.clone());
        let adapter = adapter_with_cache(old_profile).await;

        // The request goes out. This is the cache it was asked on behalf of.
        let since = adapter.chain_generation().await;

        // Mid-flight: a profile load drops the cache and republishes it complete from the new
        // profile, leaving the rate list untouched.
        adapter.invalidate_chain_cache().await;
        let winner = cache_of(
            old_modes,
            vec![filter(0, "closed-form"), filter(1, "gauss-long")],
            vec![mode(0, "ASDM7"), mode(1, "ASDM5")],
            unchanged_rates,
        );
        assert!(
            adapter
                .publish_chain_snapshot(gathered(&winner), adapter.chain_generation().await)
                .await,
            "precondition: the newer profile's set publishes, since nothing overtook it"
        );
        let winner_id = adapter
            .chain_identity()
            .await
            .expect("the winner is the cache now");

        // And only now does the overtaken reply reach the lock.
        let outcome = adapter.publish_fresh_family(stale_filters, since).await;

        assert!(
            outcome.is_err(),
            "this list was read against a cache that has since been replaced; publishing it is one \
             fault and handing it back to the setter that will resolve an index through it is the \
             other, so the fetch fails and the caller re-reads"
        );
        assert_eq!(
            adapter.chain_identity().await,
            Some(winner_id),
            "and the winner is left exactly as it was — identity and all. An overtaken fetch \
             reports its loss to its caller; it does not clear the cache, refill one family of it, \
             or leave three families from one profile beside a fourth from another"
        );
        assert_eq!(
            adapter
                .state
                .read()
                .await
                .filters
                .iter()
                .map(|f| f.name.as_str())
                .collect::<Vec<_>>(),
            vec!["closed-form", "gauss-long"],
            "which is to say: the new profile's filters, not the ones this fetch was holding"
        );
    }

    /// A concurrent full refresh can outrun a single-family fetch without changing the mapping the
    /// setter is about to use. This is the ordinary empty-cache race after connect or a mode change:
    /// the poll fills all four families while the user's `GetFilters` is in flight. Refusing that
    /// reply merely because the generation advanced turns a safe click into a 500; accepting it is
    /// safe only when the winner resolves every index to the same semantic name.
    #[tokio::test]
    async fn an_overtaken_fresh_family_that_resolves_like_the_winner_is_still_usable() {
        let winner = pcm_cache();
        let same_filters = FreshFamily::Filters(winner.filters.clone());
        let adapter = adapter_with_cache(HqpAdapterState::default()).await;

        // The setter's request starts against the empty cache.
        let since = adapter.chain_generation().await;

        // A pipeline poll wins the race and fills a complete, coherent cache from the same daemon
        // and chain before the setter's reply reaches the publisher.
        assert!(
            adapter
                .publish_chain_snapshot(gathered(&winner), since)
                .await,
            "precondition: the full refresh wins and publishes"
        );
        let winner_id = adapter
            .chain_identity()
            .await
            .expect("the winner published all four families");

        adapter
            .publish_fresh_family(same_filters, since)
            .await
            .expect(
                "the generation moved, but the winner maps every filter index to the same name; \
                 the setter can safely resolve through the reply rather than returning a 500",
            );

        assert_eq!(
            adapter.chain_identity().await,
            Some(winner_id),
            "accepting an equivalent reply must not rewrite or re-date the complete winner"
        );
    }

    /// The full-snapshot publisher's generation bump is the fence that stops a single-family fetch
    /// issued against an empty cache from clearing a newer complete winner. Unlike the profile-load
    /// races above, there is no preceding invalidation bump here: deleting the publication bump is
    /// therefore sufficient to make the stale writer pass its guard and reduce the cache to one
    /// family.
    #[tokio::test]
    async fn a_full_snapshot_fill_fences_an_older_fresh_family_with_a_different_mapping() {
        let winner = pcm_cache();
        let stale_modes = FreshFamily::Modes(vec![mode(0, "[source]"), mode(1, "SDM (DSD)")]);
        let adapter = adapter_with_cache(HqpAdapterState::default()).await;
        let since = adapter.chain_generation().await;

        assert!(
            adapter
                .publish_chain_snapshot(gathered(&winner), since)
                .await,
            "precondition: a full refresh fills the empty cache"
        );
        let winner_id = adapter
            .chain_identity()
            .await
            .expect("the full refresh published a complete winner");

        assert!(
            adapter
                .publish_fresh_family(stale_modes, since)
                .await
                .is_err(),
            "the full publication must advance the generation so an older, different mapping is \
             refused"
        );
        assert_eq!(
            adapter.chain_identity().await,
            Some(winner_id),
            "the stale family must not clear the complete winner and leave a partial cache"
        );
    }

    /// The same rule for **every** family, because the guard is one shared piece of code and a
    /// per-family spelling of it is how three of the four keep it and the fourth quietly does not.
    ///
    /// The cache event here is a bare invalidation with nothing published after it — the reconnect
    /// rather than the profile load. `connect`, `disconnect` and `mark_disconnected` all clear and
    /// leave the cache empty, and a fetch straddling one of them was answered by a session that has
    /// ended. An empty cache is not an invitation to refill one family of it with a list read
    /// through a daemon that may since have restarted, been reconfigured, or followed the source
    /// onto the other chain.
    #[tokio::test]
    async fn every_family_refuses_a_fetch_the_cache_has_outrun() {
        for fresh in each_family(&pcm_cache()) {
            let family = fresh.label();
            let adapter = adapter_with_cache(pcm_cache()).await;
            let since = adapter.chain_generation().await;

            // Mid-fetch the session dropped.
            adapter.invalidate_chain_cache().await;

            let outcome = adapter.publish_fresh_family(fresh, since).await;

            assert!(
                outcome.is_err(),
                "the {family} list was fetched against a cache that has since been cleared; it \
                 must reach neither the cache nor the setter that asked for it"
            );
            assert!(
                adapter.chain_identity().await.is_none(),
                "and the clearing stands: the {family} fetch did not refill the cache behind the \
                 back of the event that emptied it"
            );
        }
    }

    /// **A refetch that changes nothing is not a cache event, and treating it as one is not merely
    /// wasteful — it is wrong.** The generation is what an in-flight gather and an in-flight read
    /// compare against, so bumping it invalidates *their* work: a `refresh_lists` or a `fresh_*`
    /// that started a moment earlier is now told the cache moved, drops what it read and returns a
    /// failure, and `get_pipeline_status` spends its one retry on it.
    ///
    /// Every setter begins with a `fresh_*`, and the overwhelmingly common outcome of one is the
    /// list the cache already holds — the chain has not moved between two clicks. So the old
    /// unconditional bump made the *ordinary* case manufacture the very race the generation exists
    /// to detect. Byte-identical contents with no clearing in between is not a new filling of the
    /// cache; it is the same filling, confirmed.
    #[tokio::test]
    async fn a_fresh_family_identical_to_the_cached_one_is_not_a_new_filling_of_the_cache() {
        let cache = pcm_cache();
        let same_filters = FreshFamily::Filters(cache.filters.clone());
        let adapter = adapter_with_cache(cache).await;
        let before = adapter
            .chain_identity()
            .await
            .expect("a complete cache has identity");

        adapter
            .publish_fresh_family(same_filters, adapter.chain_generation().await)
            .await
            .expect("nothing overtook this fetch and the daemon served what was already cached");

        assert_eq!(
            adapter.chain_identity().await,
            Some(before),
            "the daemon confirmed the list the cache was already holding; nothing was cleared and \
             nothing was replaced, so no reader or writer in flight has been given a reason to \
             throw away what it read"
        );
    }

    /// The no-op path requires whole-family equality, not merely equality of the routing
    /// fingerprint. Filter flags are not currently projected to clients, but they are part of the
    /// daemon's enumeration and the cache must not silently discard a fresh value just because its
    /// `(index, name)` mapping stayed put.
    #[tokio::test]
    async fn changed_family_metadata_is_published_even_when_the_mapping_fingerprint_is_unchanged() {
        let cache = pcm_cache();
        let mut changed_filters = cache.filters.clone();
        changed_filters[0].value = 7;
        changed_filters[0].arg = 9;
        let adapter = adapter_with_cache(cache).await;
        let before = adapter.chain_generation().await;

        adapter
            .publish_fresh_family(FreshFamily::Filters(changed_filters.clone()), before)
            .await
            .expect("nothing overtook the fetch; only non-routing metadata changed");

        let state = adapter.state.read().await;
        assert_eq!(
            state.filters, changed_filters,
            "a fingerprint match must not turn changed family contents into a no-op"
        );
        assert_ne!(
            state.chain_generation, before,
            "publishing changed contents must fence readers that captured the earlier filling"
        );
        assert!(
            HqpAdapter::chain_identity_of(&state).is_some(),
            "the routing mapping did not move, so the other three families remain a coherent set"
        );
    }

    /// The other half of that rule, and the half that keeps it from being "never bump". A family
    /// whose fingerprint moved *is* a different chain: the other three families' indices belong to
    /// the chain that just went away, so they go with it, and the generation has to say so loudly
    /// enough that anything holding the previous set stops trusting it.
    #[tokio::test]
    async fn a_fresh_family_that_moved_clears_the_rest_and_advances_the_generation() {
        let adapter = adapter_with_cache(pcm_cache()).await;
        let before = adapter.chain_generation().await;
        let moved = FreshFamily::Filters(vec![filter(0, "closed-form"), filter(1, "gauss-long")]);

        adapter
            .publish_fresh_family(moved, before)
            .await
            .expect("nothing overtook this fetch; the daemon simply served a different list");

        assert_ne!(
            adapter.chain_generation().await,
            before,
            "the loaded chain moved under this cache, which is exactly the event an in-flight \
             gather or read must be told about"
        );
        assert!(
            adapter.chain_identity().await.is_none(),
            "and the other three families went with it: their indices name settings in the chain \
             that is no longer loaded, so the next read refills rather than mixing them with this \
             one"
        );
        assert_eq!(
            adapter
                .state
                .read()
                .await
                .filters
                .iter()
                .map(|f| f.name.as_str())
                .collect::<Vec<_>>(),
            vec!["closed-form", "gauss-long"],
            "the family that was actually fetched is published, not dropped along with the rest — \
             the caller asked for it and is about to resolve a name against it"
        );
    }

    /// The no-op short-circuit is conditioned on there being a **previous fingerprint** to match,
    /// not on the contents alone. A family the cache does not hold is a family being filled, and a
    /// filling is a cache event however familiar the list looks: the clearing that emptied it was
    /// one, and whatever was in flight across it has to see both.
    #[tokio::test]
    async fn refilling_a_cleared_family_advances_the_generation_even_with_the_same_list() {
        let cache = pcm_cache();
        let same_filters = FreshFamily::Filters(cache.filters.clone());
        let adapter = adapter_with_cache(cache).await;

        adapter.invalidate_chain_cache().await;
        let after_clear = adapter.chain_generation().await;

        adapter
            .publish_fresh_family(same_filters, after_clear)
            .await
            .expect("the fetch was issued after the clearing, so nothing has outrun it");

        assert_ne!(
            adapter.chain_generation().await,
            after_clear,
            "an empty family becoming a filled one is a filling, and the list reading the same as \
             the one cleared a moment ago does not make it the same filling"
        );
    }
}
