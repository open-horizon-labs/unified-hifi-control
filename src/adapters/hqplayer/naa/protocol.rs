//! Byte-transparent NAA relay session worker.
//!
//! Ported from the private PoC (`experiments/naa-router/native/naa-router/src/protocol.rs`, kept
//! verbatim there) with two mechanical changes: the state owner is [`NaaRelay`] instead of the
//! standalone router, and TCP keepalive uses `socket2` because this crate denies `unsafe_code`.
//! The wire delimiters/length rules are reused from the PoC's `naa-native::Parser`/`server.rs`:
//! newline XML control; 32-byte upstream binary header with sample and side-section lengths;
//! 16-byte downstream feedback. Audio is copied unchanged; nothing here decodes, repacks or
//! synthesizes capabilities. Authentication is relayed opaquely and never replayed.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::ops::Range;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use super::frame::rewrite_sections;
use super::outputs::{HqpDacDevice, HqpOutputRoute};
use super::relay::RelayCore;

const CONTROL_LIMIT: usize = 65536;
const FRAME_LIMIT: usize = 8 * 1024 * 1024;
/// Upstream audio header length (protocol 6 observed framing).
pub const HEADER_LEN: usize = 32;
/// Every record on either side is at least this long: a downstream clock/credit record is exactly
/// 16 bytes, an upstream audio header is 32 and the shortest well-formed `<networkaudio/>\n` line
/// is 16. Classifying on this prefix instead of a single '<' byte removes the ambiguity of a binary
/// record whose first byte happens to be 0x3C.
const PROBE: usize = 16;
/// Once a record has begun, the remainder must arrive within this bound.
const RECORD_TIMEOUT: Duration = Duration::from_secs(10);
/// Handshake phases (auth and internal enumeration) never wait indefinitely.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

fn invalid(s: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, s.into())
}

fn document(raw: &[u8]) -> io::Result<roxmltree::Document<'_>> {
    let text = std::str::from_utf8(raw).map_err(|_| invalid("control is not UTF-8"))?;
    if raw.len() > CONTROL_LIMIT || text.contains("<!") {
        return Err(invalid(
            "control exceeds limit or contains XML declaration constructs",
        ));
    }
    roxmltree::Document::parse(text).map_err(|e| invalid(format!("invalid control XML: {e}")))
}

/// Control lines start with one of these roots. Anything that diverges from all of them is binary.
const ROOTS: [&[u8]; 3] = [b"<networkaudio", b"<authenticate", b"<?xml"];

fn is_control(prefix: &[u8]) -> bool {
    ROOTS.iter().any(|root| prefix.starts_with(root))
}

/// Read bytes until the record is classified: `(true, root)` once a complete control root has been
/// seen, or `(false, partial)` as soon as the bytes can no longer be a control root.
///
/// `idle` bounds the wait for the first byte; `None` lets an authenticated session sit idle for as
/// long as HQPlayer keeps it open. Route changes and Stop unblock the wait by shutting the socket
/// down.
fn probe(
    reader: &mut BufReader<TcpStream>,
    idle: Option<Duration>,
) -> io::Result<(bool, Vec<u8>, Instant)> {
    reader.get_ref().set_read_timeout(idle)?;
    let mut prefix = Vec::with_capacity(PROBE);
    let mut deadline = Instant::now() + RECORD_TIMEOUT;
    loop {
        if !prefix.is_empty() {
            arm(reader, deadline)?;
        }
        let mut one = [0];
        reader.read_exact(&mut one)?;
        if prefix.is_empty() {
            deadline = Instant::now() + RECORD_TIMEOUT;
        }
        prefix.push(one[0]);
        if is_control(&prefix) {
            return Ok((true, prefix, deadline));
        }
        if !ROOTS.iter().any(|root| root.starts_with(&prefix)) {
            return Ok((false, prefix, deadline));
        }
    }
}

fn arm(reader: &BufReader<TcpStream>, deadline: Instant) -> io::Result<()> {
    let left = deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "record deadline expired"))?;
    reader.get_ref().set_read_timeout(Some(left))
}

/// Complete a binary record of `len` bytes from the classified prefix.
fn binary(
    reader: &mut BufReader<TcpStream>,
    prefix: &[u8],
    len: usize,
    deadline: Instant,
) -> io::Result<Vec<u8>> {
    let mut record = vec![0u8; len];
    record[..prefix.len()].copy_from_slice(prefix);
    let mut filled = prefix.len();
    while filled < len {
        arm(reader, deadline)?;
        let n = reader.read(&mut record[filled..])?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        filled += n;
    }
    Ok(record)
}

fn line(
    reader: &mut BufReader<TcpStream>,
    prefix: Vec<u8>,
    deadline: Instant,
) -> io::Result<Vec<u8>> {
    let mut out = prefix;
    loop {
        arm(reader, deadline)?;
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        let (chunk, done) = match buf.iter().position(|&b| b == b'\n') {
            Some(i) => (&buf[..=i], true),
            None => (buf, false),
        };
        let n = chunk.len();
        if out.len() + n > CONTROL_LIMIT {
            return Err(invalid("control line exceeds 64 KiB"));
        }
        out.extend_from_slice(chunk);
        reader.consume(n);
        if done {
            return Ok(out);
        }
    }
}

fn auth_line(reader: &mut BufReader<TcpStream>) -> io::Result<Vec<u8>> {
    let (control, prefix, deadline) = probe(reader, Some(HANDSHAKE_TIMEOUT))?;
    if !control {
        return Err(invalid("fresh authentication required"));
    }
    let raw = line(reader, prefix, deadline)?;
    if document(&raw)?.root_element().tag_name().name() != "authenticate" {
        return Err(invalid("fresh authentication required"));
    }
    Ok(raw)
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Splice byte-range edits into the original line. Ranges come from roxmltree and never overlap
/// for a well-formed document; an overlap is reported as a protocol error rather than allowed to
/// panic the session worker.
fn apply(raw: &[u8], mut edits: Vec<(Range<usize>, String)>) -> io::Result<Vec<u8>> {
    edits.sort_by_key(|(r, _)| (r.start, r.end));
    let mut out = Vec::with_capacity(raw.len());
    let mut at = 0;
    for (range, value) in edits {
        if range.start < at || range.end < range.start || range.end > raw.len() {
            return Err(invalid("overlapping control rewrite"));
        }
        out.extend_from_slice(&raw[at..range.start]);
        out.extend_from_slice(value.as_bytes());
        at = range.end;
    }
    out.extend_from_slice(&raw[at..]);
    Ok(out)
}

fn plain(a: &roxmltree::Attribute<'_, '_>) -> bool {
    a.namespace().is_none()
}

fn op<'a, 'b>(d: &'a roxmltree::Document<'b>) -> io::Result<roxmltree::Node<'a, 'b>> {
    let root = d.root_element();
    let mut children = root.children().filter(|n| n.is_element());
    let node = children
        .next()
        .ok_or_else(|| invalid("missing operation"))?;
    if root.tag_name().name() != "networkaudio"
        || node.tag_name().name() != "operation"
        || children.next().is_some()
    {
        return Err(invalid("invalid networkaudio operation"));
    }
    Ok(node)
}

/// TCP keepalive so a peer that vanished without FIN releases the exclusive session within about a
/// minute instead of holding "relay busy" until the process restarts.
fn keepalive(stream: &TcpStream) -> io::Result<()> {
    let socket = socket2::SockRef::from(stream);
    let keepalive = socket2::TcpKeepalive::new()
        .with_time(Duration::from_secs(30))
        .with_interval(Duration::from_secs(10));
    #[cfg(not(target_os = "windows"))]
    let keepalive = keepalive.with_retries(3);
    socket.set_tcp_keepalive(&keepalive)
}

/// Run one relay session to completion and release its reservation exactly once.
pub fn serve(client: TcpStream, relay: Arc<RelayCore>, id: u64, route: HqpOutputRoute) {
    // A panic must still release the exclusive session, otherwise every later HQPlayer connection
    // is refused as busy until the process restarts.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        session(client, relay.clone(), id, &route)
    }));
    let error = match outcome {
        Ok(Ok(())) => None,
        Ok(Err(e)) => Some(format!("{}: {e}", route.name)),
        Err(_) => Some(format!("{}: internal error in session worker", route.name)),
    };
    relay.finish(id, error);
}

fn session(
    client: TcpStream,
    relay: Arc<RelayCore>,
    id: u64,
    route: &HqpOutputRoute,
) -> io::Result<()> {
    client.set_nodelay(true)?;
    client.set_write_timeout(Some(Duration::from_secs(5)))?;
    keepalive(&client)?;
    let host = route.host.trim_start_matches('[').trim_end_matches(']');
    // Resolution is for this explicit route only. No discovery/endpoint fallback.
    let addresses: Vec<_> = (host, route.port).to_socket_addrs()?.take(8).collect();
    let mut result = Err(io::Error::other("selected endpoint has no addresses"));
    for addr in addresses {
        if addr.ip().is_unspecified() || addr.ip().is_multicast() {
            continue;
        }
        result = TcpStream::connect_timeout(&addr, Duration::from_secs(2));
        if result.is_ok() {
            break;
        }
    }
    let downstream = result?;
    downstream.set_nodelay(true)?;
    downstream.set_write_timeout(Some(Duration::from_secs(5)))?;
    keepalive(&downstream)?;
    relay.attach(id, &downstream)?;
    let upstream_kill = client.try_clone()?;
    let downstream_kill = downstream.try_clone()?;
    let mut up = BufReader::with_capacity(65536, client);
    let mut down = BufReader::with_capacity(65536, downstream);
    let request = auth_line(&mut up)?;
    down.get_mut().write_all(&request)?;
    relay.update(id, true, request.len(), None);
    let reply = auth_line(&mut down)?;
    up.get_mut().write_all(&reply)?;
    relay.update(id, false, reply.len(), Some("connected"));
    // HQPlayer may cache the virtual id and skip getdevices on reconnect. Resolve a blank
    // physical device id on this explicitly chosen endpoint before processing queued
    // initialization. Never pick among multiple devices.
    let mut resolved_device = route.device_id.clone().unwrap_or_default();
    if resolved_device.is_empty() {
        let request =
            b"<networkaudio><operation type=\"getdevices\" direction=\"output\"/></networkaudio>\n";
        down.get_mut().write_all(request)?;
        let (control, prefix, deadline) = probe(&mut down, Some(HANDSHAKE_TIMEOUT))?;
        if !control {
            return Err(invalid("device enumeration returned binary data"));
        }
        let raw = line(&mut down, prefix, deadline)?;
        let d = document(&raw)?;
        let operation = op(&d)?;
        if operation.attribute("type") != Some("getdevices")
            || operation.attribute("result") != Some("1")
        {
            return Err(invalid("selected NAA refused output enumeration"));
        }
        let devices: Vec<_> = operation
            .children()
            .filter(|n| n.has_tag_name("device"))
            .map(|n| HqpDacDevice {
                id: n.attribute("id").unwrap_or("").into(),
                description: n.attribute("description").unwrap_or("").into(),
            })
            .collect();
        relay.devices(id, devices.clone());
        if devices.len() != 1 || devices[0].id.is_empty() {
            return Err(invalid(format!(
                "Selected endpoint has {} outputs. Set device_id to one of the observed devices; no device was chosen automatically.",
                devices.len()
            )));
        }
        resolved_device = devices[0].id.clone();
    }
    let mut to_down = down.get_ref().try_clone()?;
    let mut to_up = up.get_ref().try_clone()?;
    let device = Arc::new(Mutex::new(resolved_device));
    let virtual_id = super::outputs::VIRTUAL_DEVICE_ID;
    let r = relay.clone();
    let selected = device.clone();
    let name = relay.name();
    let reverse = thread::spawn(move || {
        let result = downstream_loop(&mut down, &mut to_up, &r, id, &selected, &name, virtual_id);
        let _ = down.get_ref().shutdown(Shutdown::Both);
        let _ = to_up.shutdown(Shutdown::Both);
        result
    });
    let forward = upstream_loop(&mut up, &mut to_down, &relay, id, &device, virtual_id);
    let _ = upstream_kill.shutdown(Shutdown::Both);
    let _ = downstream_kill.shutdown(Shutdown::Both);
    let backward = reverse
        .join()
        .map_err(|_| io::Error::other("downstream worker panicked"))?;
    // Prefer a substantive protocol/refusal error over the opposite socket's EOF.
    if let Err(e) = backward {
        if e.kind() != io::ErrorKind::UnexpectedEof {
            return Err(e);
        }
    }
    match forward {
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(()),
        other => other,
    }
}

fn selected_device(device: &Mutex<String>) -> String {
    device
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

fn upstream_loop(
    reader: &mut BufReader<TcpStream>,
    writer: &mut TcpStream,
    relay: &RelayCore,
    id: u64,
    device: &Mutex<String>,
    virtual_id: &str,
) -> io::Result<()> {
    let mut sample_bytes = None;
    let mut injected_metadata = None;
    loop {
        let (control, prefix, deadline) = probe(reader, None)?;
        if control {
            let raw = line(reader, prefix, deadline)?;
            let d = document(&raw)?;
            if d.root_element().tag_name().name() == "authenticate" {
                // Any further authentication exchange is opaque and byte-exact.
                writer.write_all(&raw)?;
                relay.update(id, true, raw.len(), None);
                continue;
            }
            let operation = op(&d)?;
            let kind = operation.attribute("type").unwrap_or("");
            let mut edits = vec![];
            for a in operation.attributes().filter(plain) {
                if a.name() == "device" {
                    if a.value() != virtual_id {
                        return Err(invalid("HQPlayer requested an unknown virtual device"));
                    }
                    let actual = selected_device(device);
                    if actual.is_empty() {
                        return Err(invalid("No output device selected. Enumerate the selected endpoint first; choose an explicit device_id if it has multiple outputs."));
                    }
                    edits.push((a.range_value(), escape(&actual)));
                }
            }
            if kind == "getdevices" && operation.attribute("direction") != Some("output") {
                return Err(invalid("relay supports output devices only"));
            }
            if kind == "initialize" && operation.attribute("device").is_none() {
                return Err(invalid("initialize requires the virtual device"));
            }
            if kind == "start" {
                let stream = operation.attribute("stream").unwrap_or("");
                let bits = operation
                    .attribute("bits")
                    .and_then(|v| v.parse::<usize>().ok())
                    .ok_or_else(|| invalid("start has no valid bits"))?;
                injected_metadata = None;
                sample_bytes = Some(match (stream, bits) {
                    ("dsd", 1) => 1,
                    ("pcm", 8 | 16 | 24 | 32 | 64) => bits / 8,
                    _ => {
                        return Err(invalid(
                            "unqualified stream framing; only PCM 8/16/24/32/64 and native DSD supported",
                        ))
                    }
                });
            }
            let rewritten = apply(&raw, edits)?;
            // Reset before sending: a quick reply must not race this reset.
            match kind {
                "initialize" => relay.milestone(id, "initializing"),
                "start" => relay.milestone(id, "starting"),
                "stop" => relay.milestone(id, "stopped"),
                _ => {}
            }
            writer.write_all(&rewritten)?;
            relay.update(id, true, rewritten.len(), None);
        } else {
            let width =
                sample_bytes.ok_or_else(|| invalid("audio received before a framed start"))?;
            let header = binary(reader, &prefix, HEADER_LEN, deadline)?;
            let get = |offset: usize| -> io::Result<usize> {
                let bytes: [u8; 4] = header[offset..offset + 4]
                    .try_into()
                    .map_err(|_| invalid("short audio header"))?;
                Ok(u32::from_le_bytes(bytes) as usize)
            };
            let pcm_bytes = get(4)?
                .checked_mul(width)
                .ok_or_else(|| invalid("audio size overflow"))?;
            let mut remaining = pcm_bytes;
            for offset in [8, 12, 16] {
                remaining = remaining
                    .checked_add(get(offset)?)
                    .ok_or_else(|| invalid("audio size overflow"))?;
            }
            if remaining > FRAME_LIMIT - HEADER_LEN {
                return Err(invalid("audio record exceeds 8 MiB"));
            }
            // Read one complete frame so the source-independent metadata projection can replace
            // META/PIC sections. PCM/DSD remains opaque and is copied byte-for-byte.
            let mut body = Vec::with_capacity(remaining);
            let mut buffer = [0u8; 65536];
            while remaining > 0 {
                arm(reader, deadline)?;
                let want = remaining.min(buffer.len());
                let n = reader.read(&mut buffer[..want])?;
                if n == 0 {
                    return Err(io::ErrorKind::UnexpectedEof.into());
                }
                body.extend_from_slice(&buffer[..n]);
                remaining -= n;
            }
            let mut wire_header: [u8; HEADER_LEN] = header
                .try_into()
                .map_err(|_| invalid("short NAA audio header"))?;
            let metadata = relay.metadata();
            let changed = match (&metadata, &injected_metadata) {
                (Some(current), Some(previous)) => !std::sync::Arc::ptr_eq(current, previous),
                (Some(_), None) => true,
                _ => false,
            };

            let original_meta_len = u32::from_le_bytes([
                wire_header[12],
                wire_header[13],
                wire_header[14],
                wire_header[15],
            ]);
            // Native text packets can clear an endpoint's artwork: accompany those packets
            // with fallback art, but never resend artwork on every audio-only frame.
            let fallback = (changed || original_meta_len > 0)
                .then_some(metadata.as_deref())
                .flatten();
            let rewritten =
                rewrite_sections(&mut wire_header, &body, fallback, width).map_err(invalid)?;
            if original_meta_len == 0 {
                injected_metadata = metadata;
            }
            writer.write_all(&wire_header)?;
            writer.write_all(&rewritten)?;
            relay.update(id, true, HEADER_LEN + rewritten.len(), Some("forwarding"));
            if pcm_bytes > 0 {
                relay.audio(id, pcm_bytes);
            }
        }
    }
}

fn downstream_loop(
    reader: &mut BufReader<TcpStream>,
    writer: &mut TcpStream,
    relay: &RelayCore,
    id: u64,
    device: &Mutex<String>,
    name: &str,
    virtual_id: &str,
) -> io::Result<()> {
    loop {
        let (control, prefix, deadline) = probe(reader, None)?;
        let mut milestone = None;
        let output = if control {
            let raw = line(reader, prefix, deadline)?;
            let d = document(&raw)?;
            if d.root_element().tag_name().name() == "authenticate" {
                writer.write_all(&raw)?;
                relay.update(id, false, raw.len(), None);
                continue;
            }
            let operation = op(&d)?;
            let kind = operation.attribute("type").unwrap_or("");
            // Session milestones the owner waits for: a fresh successful initialize on the new
            // route, then an accepted start. Only explicit success (result="1") counts.
            if operation.attribute("result") == Some("1") {
                milestone = match kind {
                    "initialize" => Some("initialized"),
                    "start" => Some("started"),
                    _ => None,
                };
            }
            let mut edits = vec![];
            if kind == "getdevices" && operation.attribute("result") == Some("1") {
                let devices: Vec<_> = operation
                    .children()
                    .filter(|n| n.has_tag_name("device"))
                    .collect();
                relay.devices(
                    id,
                    devices
                        .iter()
                        .map(|n| HqpDacDevice {
                            id: n.attribute("id").unwrap_or("").into(),
                            description: n.attribute("description").unwrap_or("").into(),
                        })
                        .collect(),
                );
                let mut selected = device.lock().unwrap_or_else(PoisonError::into_inner);
                if selected.is_empty() {
                    if devices.len() != 1 {
                        return Err(invalid(format!(
                            "Selected endpoint has {} outputs. Set device_id to one of the observed devices; no device was chosen automatically.",
                            devices.len()
                        )));
                    }
                    *selected = devices[0].attribute("id").unwrap_or("").into();
                }
                if selected.is_empty()
                    || !devices
                        .iter()
                        .any(|n| n.attribute("id") == Some(selected.as_str()))
                {
                    return Err(invalid(
                        "configured device_id is absent from the selected NAA; inspect the observed devices",
                    ));
                }
                for n in devices {
                    if n.attribute("id") == Some(selected.as_str()) {
                        // Preserve selected device's child data and all other attrs.
                        let mut described = false;
                        for a in n.attributes().filter(plain) {
                            match a.name() {
                                "id" => edits.push((a.range_value(), escape(virtual_id))),
                                "description" => {
                                    described = true;
                                    edits.push((a.range_value(), escape(name)));
                                }
                                _ => {}
                            }
                        }
                        if !described {
                            // HQPlayer displays the description; a device without one would
                            // otherwise lose the stable relay name.
                            let after_id = n
                                .attributes()
                                .filter(plain)
                                .find(|a| a.name() == "id")
                                .map(|a| a.range().end)
                                .ok_or_else(|| invalid("device without id"))?;
                            edits.push((
                                after_id..after_id,
                                format!(" description=\"{}\"", escape(name)),
                            ));
                        }
                    } else {
                        edits.push((n.range(), String::new()));
                    }
                }
            }
            let selected = selected_device(device);
            for a in operation.attributes().filter(plain) {
                if a.name() == "device" && a.value() == selected {
                    edits.push((a.range_value(), escape(virtual_id)));
                }
            }
            apply(&raw, edits)?
        } else {
            // NAA's downstream clock/credit records are fixed 16-byte records, unlike HQPlayer's
            // 32-byte header + length-delimited payload.
            binary(reader, &prefix, PROBE, deadline)?
        };
        writer.write_all(&output)?;
        relay.update(id, false, output.len(), None);
        if let Some(m) = milestone {
            relay.milestone(id, m);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_record_starting_with_angle_bracket_is_not_control() {
        let mut feedback = [0u8; 16];
        feedback[0] = b'<';
        assert!(!is_control(&feedback));
        assert!(is_control(b"<networkaudio><o"));
        assert!(is_control(b"<?xml version=\"1"));
        assert!(is_control(b"<authenticate no"));
        assert!(!is_control(b"<device id=\"x\"/>"));
        assert!(!ROOTS.iter().any(|root| root.starts_with(b"<b")));
        assert!(ROOTS.iter().any(|root| root.starts_with(b"<a")));
        assert!(ROOTS.iter().any(|root| root.starts_with(b"<net")));
    }

    #[test]
    fn overlapping_edits_fail_instead_of_panicking() {
        let raw = b"<a x=\"1\" y=\"2\"/>";
        assert!(apply(raw, vec![(3..8, "q".into()), (5..12, "r".into())]).is_err());
        assert!(apply(raw, vec![(0..raw.len() + 1, String::new())]).is_err());
        let ok = apply(raw, vec![(6..7, "Z".into()), (12..13, "W".into())]).unwrap_or_default();
        assert_eq!(ok, b"<a x=\"Z\" y=\"W\"/>");
        let inserted =
            apply(raw, vec![(6..7, "Z".into()), (8..8, " k=\"v\"".into())]).unwrap_or_default();
        assert_eq!(inserted, b"<a x=\"Z\" k=\"v\" y=\"2\"/>");
    }
}
