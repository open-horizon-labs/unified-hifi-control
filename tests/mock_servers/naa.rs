//! Software NAA endpoint fixture and an HQPlayer-side NAA client driver.
//!
//! Ported from the private PoC's Python lab (`experiments/naa-router/tools/naa_router_lab.py`,
//! `FakeNaa` / `FakeHqpClient`). Deliberately independent of the relay implementation: it speaks
//! the observed protocol-6 framing (newline XML control, 32-byte upstream audio header with
//! sample/side-section lengths, 16-byte downstream feedback) with its own identity and a
//! fragmented writer, so byte transparency is measured rather than assumed. No vendor runtime, no
//! real DAC, no listening claim.

#![allow(dead_code)]

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub const VIRTUAL_DEVICE_ID: &str = "hiphi:router";
pub const FIXTURE_TIMEOUT: Duration = Duration::from_secs(5);

/// One upstream audio record exactly as the endpoint received it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioRecord {
    pub header: Vec<u8>,
    pub payload: Vec<u8>,
    pub position: Vec<u8>,
    pub metadata: Vec<u8>,
    pub picture: Vec<u8>,
    pub payload_sha256: String,
}

/// One observed protocol event on the endpoint side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NaaEvent {
    Auth {
        nonce: String,
        reply: Vec<u8>,
    },
    Operation {
        kind: String,
        device: Option<String>,
        raw: Vec<u8>,
    },
    Audio(AudioRecord),
    Feedback(Vec<u8>),
    Closed,
}

/// Deliberate endpoint misbehaviours a test can arm.
#[derive(Debug, Clone, Default)]
pub struct Behavior {
    /// Never answer the authentication line: the relay worker stays blocked in its read.
    pub stall_auth: bool,
    /// Answer `start` with `result="0"`.
    pub refuse_start: bool,
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

pub struct FakeNaa {
    pub name: String,
    /// Initial device list; the live list is mutable through [`FakeNaa::set_devices`].
    pub devices: Vec<(String, String)>,
    pub rate: u32,
    pub refuse_start: bool,
    live_devices: Arc<Mutex<Vec<(String, String)>>>,
    listener_addr: SocketAddr,
    stop: Arc<AtomicBool>,
    events: Arc<Mutex<Vec<NaaEvent>>>,
    connections: Arc<Mutex<Vec<TcpStream>>>,
    accept_thread: Option<JoinHandle<()>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

pub fn read_exact_n(stream: &mut impl Read, n: usize) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    stream.read_exact(&mut buf)?;
    Ok(buf)
}

pub fn read_line_bytes(reader: &mut impl BufRead) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let n = reader.read_until(b'\n', &mut out)?;
    if n == 0 {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    if out.len() > 65536 {
        return Err(io::Error::other("fixture control line too long"));
    }
    Ok(out)
}

/// Fragment at deliberately inconvenient boundaries, including XML tokens.
pub fn fragmented(stream: &mut TcpStream, data: &[u8]) -> io::Result<()> {
    let offsets = [1usize, 2, 5, 13, 31];
    let mut cursor = 0;
    for size in offsets {
        let end = (cursor + size).min(data.len());
        stream.write_all(&data[cursor..end])?;
        cursor = end;
        if cursor >= data.len() {
            return Ok(());
        }
    }
    stream.write_all(&data[cursor..])
}

pub fn control(kind: &str, attrs: &[(&str, &str)]) -> Vec<u8> {
    let mut line = format!("<networkaudio><operation type=\"{kind}\"");
    for (k, v) in attrs {
        line.push_str(&format!(" {k}=\"{}\"", escape(v)));
    }
    line.push_str("/></networkaudio>\n");
    line.into_bytes()
}

fn response(kind: &str, attrs: &[(String, String)], children: &[String]) -> Vec<u8> {
    let mut line = format!("<networkaudio><operation type=\"{kind}\" result=\"1\"");
    for (k, v) in attrs {
        if k == "type" || k == "result" {
            continue;
        }
        line.push_str(&format!(" {k}=\"{}\"", escape(v)));
    }
    if children.is_empty() {
        line.push_str("/></networkaudio>\n");
    } else {
        line.push('>');
        for child in children {
            line.push_str(child);
        }
        line.push_str("</operation></networkaudio>\n");
    }
    line.into_bytes()
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Upstream audio record: 32-byte header (`<8I`: mask, samples, pos_len, meta_len, pic_len, 3
/// reserved) followed by payload and side sections. PCM assumed 32-bit samples.
pub fn audio_record(payload: &[u8]) -> Vec<u8> {
    audio_record_with_sections(payload, &[], &[], &[])
}

/// Audio record with optional position/metadata/picture side sections, mask bits as observed.
pub fn audio_record_with_sections(
    payload: &[u8],
    position: &[u8],
    metadata: &[u8],
    picture: &[u8],
) -> Vec<u8> {
    assert!(
        payload.len() % 4 == 0,
        "pcm payload must be whole 32-bit samples"
    );
    let samples = (payload.len() / 4) as u32;
    let mask = 2u32
        | if position.is_empty() { 0 } else { 4 }
        | if metadata.is_empty() { 0 } else { 8 }
        | if picture.is_empty() { 0 } else { 16 };
    let mut record = Vec::with_capacity(32 + payload.len());
    for field in [
        mask,
        samples,
        position.len() as u32,
        metadata.len() as u32,
        picture.len() as u32,
        0,
        0,
        0,
    ] {
        record.extend_from_slice(&field.to_le_bytes());
    }
    record.extend_from_slice(payload);
    record.extend_from_slice(position);
    record.extend_from_slice(metadata);
    record.extend_from_slice(picture);
    record
}

/// Parse one control line into `(root, first operation attributes)` with a tiny scanner. Only
/// the attributes the fixture needs are read.
pub fn attribute(raw: &[u8], name: &str) -> Option<String> {
    let text = std::str::from_utf8(raw).ok()?;
    let doc = roxmltree::Document::parse(text.trim_end()).ok()?;
    let root = doc.root_element();
    if root.has_attribute(name) {
        return root.attribute(name).map(str::to_string);
    }
    root.children()
        .filter(|n| n.is_element())
        .find_map(|n| n.attribute(name).map(str::to_string))
}

pub fn root_tag(raw: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(raw).ok()?;
    let doc = roxmltree::Document::parse(text.trim_end()).ok()?;
    Some(doc.root_element().tag_name().name().to_string())
}

pub fn device_children(raw: &[u8]) -> Vec<(String, String)> {
    let Ok(text) = std::str::from_utf8(raw) else {
        return vec![];
    };
    let Ok(doc) = roxmltree::Document::parse(text.trim_end()) else {
        return vec![];
    };
    doc.descendants()
        .filter(|n| n.has_tag_name("device"))
        .map(|n| {
            (
                n.attribute("id").unwrap_or("").to_string(),
                n.attribute("description").unwrap_or("").to_string(),
            )
        })
        .collect()
}

impl FakeNaa {
    pub fn start(name: &str, device_id: &str, rate: u32) -> Self {
        Self::start_with(
            name,
            vec![(device_id.to_string(), name.to_string())],
            rate,
            false,
        )
    }

    pub fn start_with(
        name: &str,
        devices: Vec<(String, String)>,
        rate: u32,
        refuse_start: bool,
    ) -> Self {
        Self::start_with_behavior(
            name,
            devices,
            rate,
            Behavior {
                refuse_start,
                ..Behavior::default()
            },
        )
    }

    pub fn start_with_behavior(
        name: &str,
        devices: Vec<(String, String)>,
        rate: u32,
        behavior: Behavior,
    ) -> Self {
        let refuse_start = behavior.refuse_start;
        let live_devices = Arc::new(Mutex::new(devices.clone()));
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake NAA");
        listener
            .set_nonblocking(true)
            .expect("nonblocking fake NAA listener");
        let listener_addr = listener.local_addr().expect("fake NAA addr");
        let stop = Arc::new(AtomicBool::new(false));
        let events = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(Mutex::new(Vec::new()));
        let accept = {
            let stop = stop.clone();
            let events = events.clone();
            let connections = connections.clone();
            let name = name.to_string();
            let live_devices = live_devices.clone();
            thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((conn, _)) => {
                            conn.set_nonblocking(false).ok();
                            conn.set_read_timeout(Some(FIXTURE_TIMEOUT)).ok();
                            lock(&connections).push(conn.try_clone().expect("clone conn"));
                            let events = events.clone();
                            let name = name.clone();
                            let live_devices = live_devices.clone();
                            let stop = stop.clone();
                            let behavior = behavior.clone();
                            thread::spawn(move || {
                                let _ = session(
                                    conn,
                                    &name,
                                    &live_devices,
                                    rate,
                                    &behavior,
                                    &events,
                                    &stop,
                                );
                                lock(&events).push(NaaEvent::Closed);
                            });
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => break,
                    }
                }
            })
        };
        Self {
            name: name.to_string(),
            devices,
            rate,
            refuse_start,
            live_devices,
            listener_addr,
            stop,
            events,
            connections,
            accept_thread: Some(accept),
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.listener_addr
    }

    pub fn host(&self) -> String {
        self.listener_addr.ip().to_string()
    }

    pub fn port(&self) -> u16 {
        self.listener_addr.port()
    }

    pub fn events(&self) -> Vec<NaaEvent> {
        lock(&self.events).clone()
    }

    pub fn count(&self, predicate: impl Fn(&NaaEvent) -> bool) -> usize {
        lock(&self.events).iter().filter(|e| predicate(e)).count()
    }

    /// Replace the live device list for every future session (existing sessions are unaffected).
    pub fn set_devices(&self, devices: Vec<(String, String)>) {
        *lock(&self.live_devices) = devices;
    }

    pub fn auth_nonces(&self) -> Vec<String> {
        lock(&self.events)
            .iter()
            .filter_map(|e| match e {
                NaaEvent::Auth { nonce, .. } => Some(nonce.clone()),
                _ => None,
            })
            .collect()
    }

    /// The exact bytes of the most recent authentication reply this endpoint wrote.
    pub fn last_auth_reply_bytes(&self) -> Option<Vec<u8>> {
        lock(&self.events).iter().rev().find_map(|e| match e {
            NaaEvent::Auth { reply, .. } => Some(reply.clone()),
            _ => None,
        })
    }

    pub fn audio_records(&self) -> Vec<AudioRecord> {
        lock(&self.events)
            .iter()
            .filter_map(|e| match e {
                NaaEvent::Audio(record) => Some(record.clone()),
                _ => None,
            })
            .collect()
    }

    /// The exact 16 feedback bytes most recently written downstream.
    pub fn last_feedback_bytes(&self) -> Option<Vec<u8>> {
        lock(&self.events).iter().rev().find_map(|e| match e {
            NaaEvent::Feedback(bytes) => Some(bytes.clone()),
            _ => None,
        })
    }

    pub fn initialize_devices(&self) -> Vec<String> {
        lock(&self.events)
            .iter()
            .filter_map(|e| match e {
                NaaEvent::Operation { kind, device, .. } if kind == "initialize" => device.clone(),
                _ => None,
            })
            .collect()
    }

    pub fn audio_bytes(&self) -> usize {
        lock(&self.events)
            .iter()
            .map(|e| match e {
                NaaEvent::Audio(record) => record.payload.len(),
                _ => 0,
            })
            .sum()
    }

    pub fn closed_sessions(&self) -> usize {
        self.count(|e| matches!(e, NaaEvent::Closed))
    }

    pub fn wait_until(&self, predicate: impl Fn(&FakeNaa) -> bool, budget: Duration) -> bool {
        let deadline = Instant::now() + budget;
        while Instant::now() < deadline {
            if predicate(self) {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        predicate(self)
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Release);
        for conn in lock(&self.connections).drain(..) {
            let _ = conn.shutdown(Shutdown::Both);
        }
        if let Some(thread) = self.accept_thread.take() {
            let _ = thread.join();
        }
    }

    pub fn close(mut self) {
        self.shutdown();
    }
}

impl Drop for FakeNaa {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn session(
    conn: TcpStream,
    name: &str,
    live_devices: &Mutex<Vec<(String, String)>>,
    rate: u32,
    behavior: &Behavior,
    events: &Mutex<Vec<NaaEvent>>,
    stop: &AtomicBool,
) -> io::Result<()> {
    let refuse_start = behavior.refuse_start;
    let devices: Vec<(String, String)> = lock(live_devices).clone();
    let mut writer = conn.try_clone()?;
    let mut reader = BufReader::new(conn);
    let auth = read_line_bytes(&mut reader)?;
    let nonce = attribute(&auth, "nonce").unwrap_or_default();
    if behavior.stall_auth {
        lock(events).push(NaaEvent::Auth {
            nonce,
            reply: Vec::new(),
        });
        // Block until the peer goes away or the fixture is closed; never answer.
        let mut one = [0u8; 1];
        loop {
            match reader.read(&mut one) {
                Ok(0) => return Ok(()),
                Ok(_) => continue,
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    if stop.load(Ordering::Acquire) {
                        return Ok(());
                    }
                }
                Err(_) => return Ok(()),
            }
        }
    }
    // Deliberately preserve exact quotes/spacing; the relay must keep auth opaque.
    let reply = format!(
        "<authenticate  endpoint='{name}' endpoint_id='{name}-distinct-identity' nonce='{nonce}' opaque='&amp;'/>\n"
    );
    lock(events).push(NaaEvent::Auth {
        nonce,
        reply: reply.clone().into_bytes(),
    });
    fragmented(&mut writer, reply.as_bytes())?;
    let mut dsd = false;
    while !stop.load(Ordering::Acquire) {
        let first = read_exact_n(&mut reader, 1)?;
        if first[0] == b'<' {
            let mut raw = first;
            raw.extend(read_line_bytes(&mut reader)?);
            if root_tag(&raw).as_deref() == Some("authenticate") {
                let nonce = attribute(&raw, "nonce").unwrap_or_default();
                let reply = format!(
                    "<authenticate  endpoint='{name}' endpoint_id='{name}-distinct-identity' nonce='{nonce}' opaque='&amp;'/>\n"
                );
                lock(events).push(NaaEvent::Auth {
                    nonce,
                    reply: reply.clone().into_bytes(),
                });
                fragmented(&mut writer, reply.as_bytes())?;
                continue;
            }
            let kind = attribute(&raw, "type").unwrap_or_default();
            let device = attribute(&raw, "device");
            lock(events).push(NaaEvent::Operation {
                kind: kind.clone(),
                device: device.clone(),
                raw: raw.clone(),
            });
            let echo: Vec<(String, String)> = echo_attributes(&raw);
            match kind.as_str() {
                "getdevices" => {
                    let children: Vec<String> = devices
                        .iter()
                        .map(|(id, desc)| {
                            format!(
                                "<device id=\"{}\" description=\"{}\"/>",
                                escape(id),
                                escape(desc)
                            )
                        })
                        .collect();
                    fragmented(&mut writer, &response(&kind, &echo, &children))?;
                }
                "initialize" => {
                    if !devices.iter().any(|(id, _)| Some(id) == device.as_ref()) {
                        return Err(io::Error::other(format!(
                            "{name} received wrong DAC id: {device:?}"
                        )));
                    }
                    let mut attrs = echo.clone();
                    attrs.push(("version".into(), "6".into()));
                    attrs.push(("position".into(), "1".into()));
                    fragmented(&mut writer, &response(&kind, &attrs, &[]))?;
                }
                "getformats" => {
                    let children = vec![
                        format!("<format bits=\"32\" channels=\"2\" dsd=\"0\" pcm=\"1\" sdm=\"0\" rate=\"{rate}\"/>"),
                        format!("<format bits=\"1\" channels=\"2\" dsd=\"1\" pcm=\"1\" sdm=\"1\" rate=\"{}\"/>", rate * 64),
                    ];
                    fragmented(&mut writer, &response(&kind, &[], &children))?;
                }
                "start" => {
                    dsd = attribute(&raw, "stream").as_deref() == Some("dsd");
                    if refuse_start {
                        writer.write_all(b"<networkaudio><operation type=\"start\" result=\"0\" reason=\"fixture refuses format\"/></networkaudio>\n")?;
                        continue;
                    }
                    fragmented(&mut writer, &response(&kind, &echo, &[]))?;
                    fragmented(&mut writer, &[0u8; 16])?;
                }
                _ => fragmented(&mut writer, &response(&kind, &echo, &[]))?,
            }
        } else {
            let mut header = first;
            header.extend(read_exact_n(&mut reader, 31)?);
            let field = |i: usize| {
                u32::from_le_bytes([
                    header[i * 4],
                    header[i * 4 + 1],
                    header[i * 4 + 2],
                    header[i * 4 + 3],
                ]) as usize
            };
            let pcm_len = field(1) * if dsd { 1 } else { 4 };
            let (pos_len, meta_len, pic_len) = (field(2), field(3), field(4));
            let length = pcm_len + pos_len + meta_len + pic_len;
            if length > 8 * 1024 * 1024 {
                return Err(io::Error::other("fixture received unbounded record"));
            }
            let body = read_exact_n(&mut reader, length)?;
            let payload = body[..pcm_len].to_vec();
            let position = body[pcm_len..pcm_len + pos_len].to_vec();
            let metadata = body[pcm_len + pos_len..pcm_len + pos_len + meta_len].to_vec();
            let picture = body[pcm_len + pos_len + meta_len..].to_vec();
            lock(events).push(NaaEvent::Audio(AudioRecord {
                header: header.clone(),
                payload_sha256: sha256_hex(&payload),
                payload,
                position,
                metadata,
                picture,
            }));
            if (0..8).all(|i| field(i) == if i == 0 { 1 } else { 0 }) {
                continue; // observed end marker has no synthetic feedback
            }
            let mut feedback = Vec::with_capacity(16);
            for value in [0u32, rate, 0x0A3E_273C, (32 + body.len()) as u32] {
                feedback.extend_from_slice(&value.to_le_bytes());
            }
            lock(events).push(NaaEvent::Feedback(feedback.clone()));
            fragmented(&mut writer, &feedback)?;
        }
    }
    Ok(())
}

fn echo_attributes(raw: &[u8]) -> Vec<(String, String)> {
    let Ok(text) = std::str::from_utf8(raw) else {
        return vec![];
    };
    let Ok(doc) = roxmltree::Document::parse(text.trim_end()) else {
        return vec![];
    };
    doc.root_element()
        .children()
        .filter(|n| n.is_element())
        .flat_map(|n| {
            n.attributes()
                .map(|a| (a.name().to_string(), a.value().to_string()))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Drives the NAA client role HQPlayer plays against the relay: auth, getdevices, initialize,
/// getformats, then (on request) start plus a small PCM payload.
pub struct HqpNaaClient {
    stream: TcpStream,
    reader: BufReader<TcpStream>,
    last_auth_reply: Vec<u8>,
}

impl HqpNaaClient {
    pub fn connect(relay: SocketAddr, nonce: &str) -> io::Result<Self> {
        let stream = TcpStream::connect_timeout(&relay, FIXTURE_TIMEOUT)?;
        stream.set_read_timeout(Some(FIXTURE_TIMEOUT))?;
        let reader = BufReader::new(stream.try_clone()?);
        let mut client = Self {
            stream,
            reader,
            last_auth_reply: Vec::new(),
        };
        client
            .stream
            .write_all(format!("<authenticate nonce='{nonce}'/>\n").as_bytes())?;
        Ok(client)
    }

    /// Read the relayed authentication reply.
    pub fn auth_reply(&mut self) -> io::Result<Vec<u8>> {
        let reply = read_line_bytes(&mut self.reader)?;
        self.last_auth_reply = reply.clone();
        Ok(reply)
    }

    /// Exact bytes of the last relayed authentication reply.
    pub fn last_auth_reply(&self) -> Vec<u8> {
        self.last_auth_reply.clone()
    }

    /// Send one PCM record with side sections and wait for the relayed 16-byte feedback.
    pub fn send_audio_with_sections(
        &mut self,
        payload: &[u8],
        position: &[u8],
        metadata: &[u8],
    ) -> io::Result<Vec<u8>> {
        self.stream.write_all(&audio_record_with_sections(
            payload,
            position,
            metadata,
            &[],
        ))?;
        read_exact_n(&mut self.reader, 16)
    }

    /// Write a bare 32-byte audio header with the given declared lengths and no body.
    pub fn send_raw_header(
        &mut self,
        samples: u32,
        pos_len: u32,
        meta_len: u32,
        pic_len: u32,
    ) -> io::Result<()> {
        let mut header = Vec::with_capacity(32);
        for field in [2u32, samples, pos_len, meta_len, pic_len, 0, 0, 0] {
            header.extend_from_slice(&field.to_le_bytes());
        }
        self.stream.write_all(&header)
    }

    pub fn getdevices(&mut self) -> io::Result<Vec<(String, String)>> {
        self.stream
            .write_all(&control("getdevices", &[("direction", "output")]))?;
        let reply = read_line_bytes(&mut self.reader)?;
        Ok(device_children(&reply))
    }

    pub fn initialize(&mut self) -> io::Result<Vec<u8>> {
        self.stream.write_all(&control(
            "initialize",
            &[
                ("device", VIRTUAL_DEVICE_ID),
                ("direction", "output"),
                ("channels", "2"),
                ("channel_offset", "0"),
                ("pack_sdm", "0"),
                ("periodtime", "250"),
                ("low_delay", "0"),
                ("version_req", "5"),
            ],
        ))?;
        read_line_bytes(&mut self.reader)
    }

    pub fn getformats(&mut self) -> io::Result<Vec<u8>> {
        self.stream.write_all(&control("getformats", &[]))?;
        read_line_bytes(&mut self.reader)
    }

    /// Full handshake through the relay. Returns the initialize reply.
    pub fn handshake(&mut self) -> io::Result<Vec<u8>> {
        self.auth_reply()?;
        self.getdevices()?;
        let init = self.initialize()?;
        self.getformats()?;
        Ok(init)
    }

    pub fn start(&mut self, rate: u32) -> io::Result<Vec<u8>> {
        self.stream.write_all(&control(
            "start",
            &[
                ("bits", "32"),
                ("channels", "2"),
                ("netbuftime", "1"),
                ("rate", &rate.to_string()),
                ("stream", "pcm"),
            ],
        ))?;
        let reply = read_line_bytes(&mut self.reader)?;
        if attribute(&reply, "result").as_deref() == Some("1") {
            read_exact_n(&mut self.reader, 16)?;
        }
        Ok(reply)
    }

    /// Send one PCM record and wait for the relayed 16-byte feedback.
    pub fn send_audio(&mut self, payload: &[u8]) -> io::Result<Vec<u8>> {
        self.stream.write_all(&audio_record(payload))?;
        read_exact_n(&mut self.reader, 16)
    }

    /// Whether the relay closed this connection (read returns EOF or reset within the timeout).
    pub fn is_closed(&mut self) -> bool {
        let mut one = [0u8; 1];
        match self.stream.read(&mut one) {
            Ok(0) => true,
            Ok(_) => false,
            Err(e) => matches!(
                e.kind(),
                io::ErrorKind::ConnectionReset
                    | io::ErrorKind::BrokenPipe
                    | io::ErrorKind::UnexpectedEof
                    | io::ErrorKind::ConnectionAborted
            ),
        }
    }

    pub fn set_read_timeout(&mut self, timeout: Duration) {
        let _ = self.stream.set_read_timeout(Some(timeout));
    }
}

/// Whether anything answers on `addr` right now (connect succeeds).
pub fn port_is_listening(addr: SocketAddr) -> bool {
    TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
}

/// A loopback port nobody is listening on (bind then release).
pub fn reserved_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("reserve port");
    listener.local_addr().expect("reserved addr").port()
}

/// Drives the NAA client role HQPlayer Embedded itself plays against the relay, with its settled
/// autoreconnect behaviour: whenever its session closes (a route switch), it reconnects and
/// performs a fresh auth/getdevices/initialize/getformats handshake on its own, then waits. It only
/// sends `start` plus a small PCM payload once `permit_start` is set (modelling that a control-plane
/// Play is what makes the engine stream), and keeps streaming records while permitted.
pub struct AutoHqpClient {
    permit: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    events: Arc<Mutex<Vec<(String, u64)>>>,
    /// Every 16-byte feedback record the relay delivered back to this client, in order.
    feedback: Arc<Mutex<Vec<Vec<u8>>>>,
    thread: Option<JoinHandle<()>>,
}

/// The known PCM payload every streamed record carries (256 bytes, 64 little-endian u32s).
pub fn auto_client_payload() -> Vec<u8> {
    (0..64u32).flat_map(|i| i.to_le_bytes()).collect()
}

impl AutoHqpClient {
    pub fn start(relay: SocketAddr, rate: u32) -> Self {
        let permit = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let events = Arc::new(Mutex::new(Vec::new()));
        let feedback = Arc::new(Mutex::new(Vec::new()));
        let thread = {
            let permit = permit.clone();
            let stop = stop.clone();
            let events = events.clone();
            let feedback = feedback.clone();
            thread::spawn(move || {
                let mut generation = 0u64;
                while !stop.load(Ordering::Acquire) {
                    generation += 1;
                    let _ =
                        Self::session(relay, rate, generation, &permit, &stop, &events, &feedback);
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    thread::sleep(Duration::from_millis(50));
                }
            })
        };
        Self {
            permit,
            stop,
            events,
            feedback,
            thread: Some(thread),
        }
    }

    fn session(
        relay: SocketAddr,
        rate: u32,
        generation: u64,
        permit: &AtomicBool,
        stop: &AtomicBool,
        events: &Mutex<Vec<(String, u64)>>,
        feedback: &Mutex<Vec<Vec<u8>>>,
    ) -> io::Result<()> {
        let mut client = HqpNaaClient::connect(relay, &format!("auto-reconnect-{generation}"))?;
        client.set_read_timeout(Duration::from_millis(200));
        // The relay closes immediately when no route is selected; a closed auth read is normal.
        let mut wait = 0u32;
        let auth = loop {
            match client.auth_reply() {
                Ok(reply) => break reply,
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    wait += 1;
                    if wait > 25 || stop.load(Ordering::Acquire) {
                        return Ok(());
                    }
                }
                Err(e) => return Err(e),
            }
        };
        let _ = auth;
        client.set_read_timeout(FIXTURE_TIMEOUT);
        lock(events).push(("connected".into(), generation));
        client.getdevices()?;
        let init = client.initialize()?;
        if attribute(&init, "result").as_deref() != Some("1") {
            lock(events).push(("initialize_failed".into(), generation));
            return Ok(());
        }
        client.getformats()?;
        lock(events).push(("initialized".into(), generation));
        // Wait for an external Play signal without missing a relay-initiated close.
        client.set_read_timeout(Duration::from_millis(100));
        while !permit.load(Ordering::Acquire) {
            if stop.load(Ordering::Acquire) {
                return Ok(());
            }
            let mut one = [0u8; 1];
            match client.stream.read(&mut one) {
                Ok(0) => return Ok(()),
                Ok(_) => {}
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) => {}
                Err(_) => return Ok(()),
            }
        }
        client.set_read_timeout(FIXTURE_TIMEOUT);
        let start = client.start(rate)?;
        lock(events).push(("start".into(), generation));
        if attribute(&start, "result").as_deref() != Some("1") {
            lock(events).push(("start_refused".into(), generation));
            return Ok(());
        }
        // Stream while permitted; each record is a real payload with relayed feedback.
        let payload = auto_client_payload();
        while permit.load(Ordering::Acquire) && !stop.load(Ordering::Acquire) {
            let fb = client.send_audio(&payload)?;
            lock(feedback).push(fb);
            lock(events).push(("audio".into(), generation));
            thread::sleep(Duration::from_millis(20));
        }
        // Playback stopped from the control plane: keep the session open (as Embedded does) until
        // the relay closes it or Play is permitted again.
        client.set_read_timeout(Duration::from_millis(100));
        while !stop.load(Ordering::Acquire) {
            if permit.load(Ordering::Acquire) {
                client.set_read_timeout(FIXTURE_TIMEOUT);
                let start = client.start(rate)?;
                lock(events).push(("start".into(), generation));
                if attribute(&start, "result").as_deref() != Some("1") {
                    return Ok(());
                }
                while permit.load(Ordering::Acquire) && !stop.load(Ordering::Acquire) {
                    let fb = client.send_audio(&payload)?;
                    lock(feedback).push(fb);
                    lock(events).push(("audio".into(), generation));
                    thread::sleep(Duration::from_millis(20));
                }
                client.set_read_timeout(Duration::from_millis(100));
                continue;
            }
            let mut one = [0u8; 1];
            match client.stream.read(&mut one) {
                Ok(0) => return Ok(()),
                Ok(_) => {}
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) => {}
                Err(_) => return Ok(()),
            }
        }
        Ok(())
    }

    /// Model the control plane: streaming begins only while this is set.
    pub fn set_playing(&self, playing: bool) {
        self.permit.store(playing, Ordering::Release);
    }

    pub fn events(&self) -> Vec<(String, u64)> {
        lock(&self.events).clone()
    }

    pub fn count(&self, kind: &str) -> usize {
        lock(&self.events).iter().filter(|(k, _)| k == kind).count()
    }

    /// Feedback records received so far, in order.
    pub fn feedback(&self) -> Vec<Vec<u8>> {
        lock(&self.feedback).clone()
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.permit.store(false, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            // Every blocking read in the session loop is bounded (≤ FIXTURE_TIMEOUT), so this join
            // is bounded too.
            let _ = thread.join();
        }
    }

    pub fn close(mut self) {
        self.shutdown();
    }
}

impl Drop for AutoHqpClient {
    /// RAII: an assertion failure before `close` must not leak a reconnecting thread.
    fn drop(&mut self) {
        self.shutdown();
    }
}
