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
            modes_fingerprint: None,
            filters_fingerprint: None,
            shapers_fingerprint: None,
            rates_fingerprint: None,
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

        // A new session inherits nothing. The daemon may have restarted onto another profile, been
        // reconfigured, or simply followed the source onto the other chain while UHC was away, and a
        // list index cached before the drop names a different setting in every one of those cases.
        self.invalidate_chain_cache().await;

        let (read_half, write_half) = stream.into_split();
        let reader = BufReader::new(read_half);

        {
            let mut conn = self.connection.lock().await;
            *conn = Some(HqpConnection {
                stream: reader,
                write_half,
                carry: Vec::new(),
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
        // Nothing observed through the old session may outlive it.
        self.invalidate_chain_cache().await;

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
        state.modes.clear();
        state.filters.clear();
        state.shapers.clear();
        state.rates.clear();
        state.modes_fingerprint = None;
        state.filters_fingerprint = None;
        state.shapers_fingerprint = None;
        state.rates_fingerprint = None;
        tracing::debug!("Invalidated HQPlayer chain-scoped enumeration cache");
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

    /// Record a freshly fetched enumeration and report whether the loaded chain moved.
    ///
    /// The comparison is against the **previous fingerprint of the same family**; a family never
    /// observed before cannot be a transition. When one does move, every other chain-scoped family is
    /// dropped rather than refetched here: the caller may not need them, and a lazy refill keeps a
    /// control path to one enumeration fetch.
    async fn note_enumeration(&self, family: &str, fingerprint: u64) -> bool {
        let previous = {
            let state = self.state.read().await;
            match family {
                "modes" => state.modes_fingerprint,
                "filters" => state.filters_fingerprint,
                "shapers" => state.shapers_fingerprint,
                _ => state.rates_fingerprint,
            }
        };
        let changed = previous.is_some_and(|p| p != fingerprint);
        if changed {
            tracing::info!(
                "HQPlayer {family} enumeration changed: the loaded chain moved, dropping every \
                 chain-scoped list"
            );
            self.invalidate_chain_cache().await;
        }
        changed
    }

    /// Fetch the modes list from the daemon and cache it, noticing a device change.
    async fn fresh_modes(&self) -> Result<Vec<ListItem>> {
        let modes = self.get_modes().await?;
        let fingerprint = Self::fingerprint(modes.iter().map(|m| (m.index, m.name.as_str())));
        self.note_enumeration("modes", fingerprint).await;
        let mut state = self.state.write().await;
        state.modes = modes.clone();
        state.modes_fingerprint = Some(fingerprint);
        Ok(modes)
    }

    /// Fetch the loaded chain's filter list from the daemon and cache it.
    async fn fresh_filters(&self) -> Result<Vec<FilterItem>> {
        let filters = self.get_filters().await?;
        let fingerprint = Self::fingerprint(filters.iter().map(|f| (f.index, f.name.as_str())));
        self.note_enumeration("filters", fingerprint).await;
        let mut state = self.state.write().await;
        state.filters = filters.clone();
        state.filters_fingerprint = Some(fingerprint);
        Ok(filters)
    }

    /// Fetch the loaded chain's shaper list from the daemon and cache it.
    async fn fresh_shapers(&self) -> Result<Vec<ListItem>> {
        let shapers = self.get_shapers().await?;
        let fingerprint = Self::fingerprint(shapers.iter().map(|s| (s.index, s.name.as_str())));
        self.note_enumeration("shapers", fingerprint).await;
        let mut state = self.state.write().await;
        state.shapers = shapers.clone();
        state.shapers_fingerprint = Some(fingerprint);
        Ok(shapers)
    }

    /// Fetch the loaded chain's rate list from the daemon and cache it.
    ///
    /// Also the **chain probe** used by the read path: it is the smallest chain-scoped enumeration and
    /// the two chains' rate lists were observed disjoint (HQP-C-020, `E0-uhc-live`). The residual is
    /// stated rather than hidden: the offered rates also depend on the selected filter (HQP-C-021), so
    /// a filter change can move this fingerprint without the chain having moved. That direction is
    /// harmless — it costs a refetch of lists that were about to be re-read anyway. The opposite
    /// direction, a chain move that leaves the rate list byte-identical, is what would be missed, and
    /// no observation suggests it is reachable.
    async fn fresh_rates(&self) -> Result<Vec<RateItem>> {
        let rates = self.get_rates().await?;
        let labels: Vec<String> = rates.iter().map(|r| r.rate.to_string()).collect();
        let fingerprint = Self::fingerprint(
            rates
                .iter()
                .zip(labels.iter())
                .map(|(r, label)| (r.index, label.as_str())),
        );
        self.note_enumeration("rates", fingerprint).await;
        let mut state = self.state.write().await;
        state.rates = rates.clone();
        state.rates_fingerprint = Some(fingerprint);
        Ok(rates)
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

    /// Ask whether the loaded chain has moved, and **publish nothing either way**.
    ///
    /// The read path's probe. Deliberately not [`Self::fresh_rates`], which caches what it fetched:
    /// caching here would publish one family on its own, so a poll that detected a transition would
    /// leave a caller holding the new chain's rates beside no filters and no shapers — a partial view,
    /// offered as a whole one. Detection and publication are separate jobs, and [`Self::refresh_lists`]
    /// owns the second.
    async fn chain_probe(&self) -> Result<()> {
        let fingerprint = Self::rates_fingerprint(&self.get_rates().await?);
        let previous = self.state.read().await.rates_fingerprint;
        if previous.is_some_and(|p| p != fingerprint) {
            tracing::info!(
                "HQPlayer rate enumeration changed: the loaded chain moved, dropping every \
                 chain-scoped list"
            );
            self.invalidate_chain_cache().await;
        }
        Ok(())
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
    async fn refresh_lists(&self) {
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
                return;
            }
        };

        let rates_fp = Self::rates_fingerprint(&rates);
        if Self::rates_fingerprint(&opening) != rates_fp {
            tracing::warn!(
                "HQPlayer's loaded chain moved while its lists were being read; publishing nothing \
                 rather than a set that straddles the change"
            );
            return;
        }

        let modes_fp = Self::fingerprint(modes.iter().map(|m| (m.index, m.name.as_str())));
        let filters_fp = Self::fingerprint(filters.iter().map(|f| (f.index, f.name.as_str())));
        let shapers_fp = Self::fingerprint(shapers.iter().map(|s| (s.index, s.name.as_str())));

        tracing::debug!(
            "Refreshed HQPlayer lists: {} modes, {} filters, {} shapers, {} rates",
            modes.len(),
            filters.len(),
            shapers.len(),
            rates.len()
        );

        let mut state = self.state.write().await;
        state.modes = modes;
        state.filters = filters;
        state.shapers = shapers;
        state.rates = rates;
        state.modes_fingerprint = Some(modes_fp);
        state.filters_fingerprint = Some(filters_fp);
        state.shapers_fingerprint = Some(shapers_fp);
        state.rates_fingerprint = Some(rates_fp);
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
        self.invalidate_chain_cache().await;

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
        let timeouts = self.timeouts().await;
        let mut last_error = None;
        // Relative and toggling commands carry a side effect that is not the same applied twice.
        let one_shot = framing::root_element(xml)
            .map(|e| Self::is_one_shot(&e))
            .unwrap_or(false);

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
        let mut conn_guard = self.connection.lock().await;
        let conn = conn_guard
            .as_mut()
            .ok_or_else(|| anyhow!("Not connected"))?;

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

    /// Confirm a setting actually applied, by reading the authoritative `State` field back.
    ///
    /// Verified daemon behaviour: a setter can return `result="OK"` without the setting actually
    /// applying, so `OK` alone is never proof (HQP-C-028). A change can also land a poll later than the
    /// acknowledgement, so this polls rather than checking once, reusing the injected retry policy
    /// instead of introducing another knob.
    ///
    /// Returns [`SettingOutcome::Ignored`] rather than an error when the value never appears: a write
    /// the daemon acknowledged and dropped is a different fact from one it rejected, and a caller that
    /// wants them collapsed asks for that with [`SettingOutcome::into_applied_result`].
    ///
    /// When the authoritative field is **absent** rather than merely different, the answer is
    /// [`SettingOutcome::Ambiguous`] instead. Those are not the same fact: a field reporting another
    /// value proves the write did not take, while a field the daemon does not report at all proves
    /// nothing either way, and calling the second "ignored" would assert a failure that was never
    /// observed.
    ///
    /// `observe` must read the field that *is* the setting. It deliberately has no fallback to a
    /// related field: reading a sibling when the authoritative one is absent is how a readback confirms
    /// something it never checked.
    async fn verify_applied<T, F>(
        &self,
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
            let state = self.get_state().await?;
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

    /// Write a setting and establish what it did, including when the reply never arrives.
    ///
    /// Three outcomes come out of the send itself:
    ///
    /// * a reply — the ordinary path, and readback decides between `Applied` and `Ignored`;
    /// * an explicit [`HqpRejected`] — terminal, propagated as the error it is, nothing changed;
    /// * anything else (timeout, dropped connection, malformed reply) — **ambiguous delivery**. On
    ///   HQPlayer Embedded 6.0.4 a `SetMode` was accepted, logged and acted on while the daemon sent
    ///   no response and later dropped the connection (HQP-C-029), so treating silence as failure is
    ///   as wrong as treating `OK` as success. The state is read back regardless: if the setting is
    ///   there, the write landed and is reported as applied; if it is not, the outcome is
    ///   `Ambiguous` — which is neither a success nor a claim that nothing happened.
    ///
    /// The readback reconnects on its own (`get_state` goes through `ensure_connected`), so this is
    /// also the recovery path, not just the reporting one.
    ///
    /// Only absolute writes reach here. A relative one-shot is never retried after its write is
    /// attempted (HQP-C-030), and `send_command` enforces that separately.
    async fn write_setting<T, F>(
        &self,
        xml: &str,
        what: &str,
        expected: T,
        observe: F,
    ) -> Result<SettingOutcome>
    where
        T: PartialEq + std::fmt::Display + Clone,
        F: Fn(&HqpState) -> Option<T>,
    {
        match self.send_command(xml).await {
            Ok(_) => self.verify_applied(what, expected, observe).await,
            Err(e) if e.downcast_ref::<HqpRejected>().is_some() => Err(e),
            Err(e) => {
                tracing::warn!(
                    "HQPlayer {what} write got no usable reply ({e}); reading the state back, \
                     because a lost reply is not proof the daemon did not apply it"
                );
                match self.verify_applied(what, expected, observe).await {
                    Ok(SettingOutcome::Applied) => Ok(SettingOutcome::Applied),
                    _ => Ok(SettingOutcome::Ambiguous {
                        what: what.to_string(),
                        reason: format!(
                            "the write was attempted but drew no usable reply ({e}), and the \
                             daemon's state does not show it; it may or may not have been applied"
                        ),
                    }),
                }
            }
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
    pub async fn set_mode(&self, mode_name: &str) -> Result<SettingOutcome> {
        let modes = self.fresh_modes().await?;
        let mode_index = Self::resolve_mode(&modes, mode_name)?;

        let state = self.get_state().await?;
        if u32::from(state.mode) == mode_index {
            tracing::debug!(
                "HQPlayer is already in mode {mode_index}; not writing it, because SetMode clears \
                 the rate pin even when the mode does not change"
            );
            return Ok(SettingOutcome::AlreadySet);
        }

        let xml = Self::build_request("SetMode", &[("value", &mode_index.to_string())]);
        let outcome = self
            .write_setting(&xml, "mode", mode_index, |s| Some(u32::from(s.mode)))
            .await?;
        // A mode change reloads the chain whatever the readback said, so nothing cached about it
        // survives. This is also what makes the cleared pin honest: there is no remembered rate to
        // report, only the one the daemon now holds.
        self.invalidate_chain_cache().await;
        Ok(outcome)
    }

    /// Send `SetFilter` with **both** arguments.
    ///
    /// The daemon's `SetFilter` writes both sides: `value` alone sets 1x and Nx together, and
    /// `value1x` splits them. There is therefore no such thing as a one-sided write on the wire — a
    /// caller that omits the sibling is overwriting it with whatever it passed as `value`. Taking both
    /// indices is what makes that impossible to express, rather than merely discouraged.
    ///
    /// **Indices in, and no readback.** This is the raw form: it resolves no name and confirms
    /// nothing, so it is not a control path. Every advertised surface goes through
    /// [`Self::set_filter_1x`] or [`Self::set_filter_nx`], which resolve a semantic name against the
    /// loaded chain's list and verify the result.
    /// The request both sides of the pair travel in, built in one place so they cannot drift.
    fn filter_request(value_nx: u32, value1x: u32) -> String {
        let nx = value_nx.to_string();
        let one_x = value1x.to_string();
        Self::build_request("SetFilter", &[("value", &nx), ("value1x", &one_x)])
    }

    pub async fn set_filter(&self, value_nx: u32, value1x: u32) -> Result<()> {
        tracing::debug!("SetFilter: value={value_nx} (Nx), value1x={value1x} (1x)");
        self.send_command(&Self::filter_request(value_nx, value1x))
            .await?;
        Ok(())
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
        let filters = self.fresh_filters().await?;
        let index = Self::resolve_filter(&filters, filter_name)?;

        let state = self.get_state().await?;
        if state.filter1x == Some(index) && state.filter_nx == Some(index) {
            return Ok(SettingOutcome::AlreadySet);
        }

        // Both sides are the setting here, so both are verified. Rendered as one value because the
        // pair is what was requested and reporting half of it back would be the same mistake as
        // writing half of it.
        let expected = format!("1x={index},Nx={index}");
        tracing::debug!("SetFilter (both sides): value={index}, value1x={index}");
        let xml = Self::filter_request(index, index);
        self.write_setting(&xml, "filter", expected, |s| {
            match (s.filter1x, s.filter_nx) {
                (Some(one_x), Some(nx)) => Some(format!("1x={one_x},Nx={nx}")),
                // Absent rather than mismatched: `verify_applied` turns a field the daemon never
                // reports into `Ambiguous`, which is the truthful answer when nothing can be seen.
                _ => None,
            }
        })
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
        let filters = self.fresh_filters().await?;
        let index = Self::resolve_filter(&filters, filter_name)?;
        let what = side.field();

        let state = self.get_state().await?;
        let (Some(current_1x), Some(current_nx)) = (state.filter1x, state.filter_nx) else {
            return Ok(SettingOutcome::Suppressed {
                what: what.to_string(),
                reason: "the daemon's State does not report both filter1x and filterNx, and \
                         SetFilter writes both sides at once; sending it would overwrite the sibling \
                         with a guess"
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
        self.write_setting(&xml, what, index, |s| match side {
            FilterSide::OneX => s.filter1x,
            FilterSide::Nx => s.filter_nx,
        })
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
        let shapers = self.fresh_shapers().await?;
        let shaper_index = Self::resolve_shaper(&shapers, shaper_name)?;

        if self.get_state().await?.shaper == shaper_index {
            return Ok(SettingOutcome::AlreadySet);
        }

        let xml = Self::build_request("SetShaping", &[("value", &shaper_index.to_string())]);
        self.write_setting(&xml, "shaper", shaper_index, |s| Some(s.shaper))
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
        let modes = self.fresh_modes().await?;
        let state = self.get_state().await?;
        if let Some(mode_name) = Self::configured_source_mode(&modes, &state) {
            return Ok(SettingOutcome::Suppressed {
                what: "rate".to_string(),
                reason: format!(
                    "the configured mode is '{mode_name}', in which the daemon acknowledges a rate \
                     pin and applies nothing — the rate is governed by persistent configuration \
                     there, so the write was not sent"
                ),
            });
        }

        let rates = self.fresh_rates().await?;
        let index = rates
            .iter()
            .find(|r| r.rate == rate_value)
            .map(|r| r.index)
            .ok_or_else(|| {
                anyhow!(
                    "Rate {} is not offered for the chain this daemon has loaded",
                    rate_value
                )
            })?;

        if state.rate == index {
            return Ok(SettingOutcome::AlreadySet);
        }

        let xml = Self::build_request("SetRate", &[("value", &index.to_string())]);
        self.write_setting(&xml, "rate", index, |s| Some(s.rate))
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

        // Chain probe. The lists this view publishes are scoped to the chain the daemon has *loaded*,
        // and under `[source]` that chain moves without `State.mode` moving at all (HQP-C-007) — so
        // "refresh when a list is empty" served the previous chain's options indefinitely, and a UI
        // offering PCM filters over a loaded DSD chain is a lie a user acts on.
        //
        // The rate list is the probe because it is the smallest chain-scoped enumeration and the two
        // chains' rate lists were observed disjoint (HQP-C-020). Its fingerprint changing drops every
        // chain-scoped cache, which the lazy fill below then repopulates. A probe failure is not fatal
        // to a read: the caller still gets `State` and `Status`.
        if let Err(e) = self.chain_probe().await {
            tracing::warn!("HQPlayer chain probe failed, serving cached enumerations: {e}");
        }

        // Lazy-fill whatever the probe left empty — on the first read after connect, that is all of it.
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
        let current = self.get_state().await?.matrix_profile;
        if current.is_empty() {
            // No selection. The empty field is the default identity and is not list position 0.
            return Ok(None);
        }
        let profiles = self.get_matrix_profiles().await?;
        let resolved = profiles.into_iter().find(|p| p.name == current);
        if resolved.is_none() {
            tracing::warn!(
                "HQPlayer reports matrix profile {current:?}, which its own MatrixListProfiles does \
                 not contain; reporting no selection rather than another profile's index"
            );
        }
        Ok(resolved)
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
        let profiles = self.get_matrix_profiles().await?;
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
        self.set_matrix_profile_named(&name).await
    }

    /// Select a matrix profile by name, verifying against the authoritative `State` field.
    ///
    /// `MatrixSetProfile` takes the name, and like every other setter its `result="OK"` is not proof
    /// of application (HQP-C-028) — so the write is confirmed by reading `State.matrix_profile` back.
    pub async fn set_matrix_profile_named(&self, name: &str) -> Result<SettingOutcome> {
        if self.get_state().await?.matrix_profile == name {
            return Ok(SettingOutcome::AlreadySet);
        }
        let xml = Self::build_request("MatrixSetProfile", &[("value", name)]);
        self.write_setting(&xml, "matrix profile", name.to_string(), |s| {
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
