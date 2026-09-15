#[cfg(any(target_os = "android", target_os = "linux"))]
use std::os::unix::io::FromRawFd;
// Private NAA transport: only device POSITION supplies rendered progress.
use crate::pcm::{Offer, PcmFormat};
use std::{
    collections::{BTreeMap, VecDeque},
    env,
    io::{self, Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    os::unix::{io::AsRawFd, net::UnixStream},
    time::{Duration, Instant},
};
const LIMIT: usize = 1024 * 1024;
const MAX_RECORD: usize = 8 * LIMIT;
const DEVICE: &str = "hw:CARD=ANDROIDNAA,DEV=0";
fn err(s: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, s)
}
fn escaped(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
fn document(raw: &[u8]) -> io::Result<roxmltree::Document<'_>> {
    let s = std::str::from_utf8(raw).map_err(|_| err("XML_UTF8"))?;
    if s.len() > 65536 || s.contains("<!") {
        return Err(err("XML_LIMIT_OR_DECLARATION"));
    }
    roxmltree::Document::parse(s).map_err(|_| err("XML_INVALID"))
}
fn operation(raw: &[u8]) -> io::Result<(String, BTreeMap<String, String>)> {
    let d = document(raw)?;
    let root = d.root_element();
    if root.tag_name().name() != "networkaudio" {
        return Err(err("CONTROL_ROOT"));
    }
    let children: Vec<_> = root.children().filter(|n| n.is_element()).collect();
    if children.len() != 1 || children[0].tag_name().name() != "operation" {
        return Err(err("CONTROL_OPERATION"));
    }
    let op = children[0];
    let kind = op
        .attribute("type")
        .ok_or_else(|| err("CONTROL_TYPE"))?
        .to_string();
    let attrs = op
        .attributes()
        .map(|a| (a.name().to_string(), a.value().to_string()))
        .collect();
    Ok((kind, attrs))
}
fn response(kind: &str, attrs: &BTreeMap<String, String>, body: &str, success: bool) -> Vec<u8> {
    let mut s = format!(
        "<networkaudio><operation type=\"{}\" result=\"{}\"",
        escaped(kind),
        if success { 1 } else { 0 }
    );
    for (k, v) in attrs {
        if k != "type" && k != "result" {
            s.push_str(&format!(" {}=\"{}\"", k, escaped(v)));
        }
    }
    s.push('>');
    s.push_str(body);
    s.push_str("</operation></networkaudio>\n");
    s.into_bytes()
}
fn read_line(s: &mut TcpStream, deadline: Instant) -> io::Result<Vec<u8>> {
    let mut b = Vec::new();
    loop {
        let left = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| err("AUTH_TIMEOUT"))?;
        s.set_read_timeout(Some(left))?;
        let mut one = [0];
        s.read_exact(&mut one)?;
        b.push(one[0]);
        if b.len() > 65536 {
            return Err(err("AUTH_LIMIT"));
        }
        if one[0] == b'\n' {
            return Ok(b);
        }
    }
}
fn authenticate(s: &mut TcpStream, port: u16) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let request = read_line(s, deadline)?;
    if document(&request)?.root_element().tag_name().name() != "authenticate" {
        return Err(err("FRESH_AUTH_REQUIRED"));
    }
    let mut helper =
        TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_secs(2))?;
    helper.set_write_timeout(Some(Duration::from_secs(2)))?;
    helper.write_all(&request)?;
    let reply = read_line(&mut helper, deadline)?;
    if document(&reply)?.root_element().tag_name().name() != "authenticate" {
        return Err(err("AUTH_REPLY_INVALID"));
    }
    helper.shutdown(Shutdown::Both)?;
    drop(helper);
    s.set_write_timeout(Some(Duration::from_secs(2)))?;
    s.write_all(&reply)?;
    s.set_read_timeout(None)?;
    s.set_write_timeout(None)?;
    Ok(())
}
fn connect_ipc(name: &str) -> io::Result<UnixStream> {
    if !name.starts_with('@') {
        return UnixStream::connect(name);
    }
    #[cfg(any(target_os = "android", target_os = "linux"))]
    unsafe {
        let bytes = &name.as_bytes()[1..];
        if bytes.is_empty() || bytes.len() > 106 {
            return Err(err("IPC_NAME"));
        }
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut addr: libc::sockaddr_un = std::mem::zeroed();
        addr.sun_family = libc::AF_UNIX as _;
        std::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            addr.sun_path.as_mut_ptr().cast::<u8>().add(1),
            bytes.len(),
        );
        if libc::connect(
            fd,
            &addr as *const _ as *const libc::sockaddr,
            (2 + 1 + bytes.len()) as _,
        ) != 0
        {
            let e = io::Error::last_os_error();
            libc::close(fd);
            return Err(e);
        }
        Ok(UnixStream::from_raw_fd(fd))
    }
    #[cfg(not(any(target_os = "android", target_os = "linux")))]
    {
        Err(err("ABSTRACT_SOCKET_ANDROID_ONLY"))
    }
}
#[derive(Default)]
struct Buffer {
    input: Vec<u8>,
    out: VecDeque<Vec<u8>>,
    offset: usize,
    queued: usize,
}
impl Buffer {
    fn push(&mut self, bytes: Vec<u8>) -> io::Result<()> {
        if bytes.len() > 2 * LIMIT - self.queued {
            return Err(err("OUTPUT_LIMIT"));
        }
        self.queued += bytes.len();
        self.out.push_back(bytes);
        Ok(())
    }
    fn flush(&mut self, w: &mut impl Write) -> io::Result<()> {
        while let Some(bytes) = self.out.front() {
            match w.write(&bytes[self.offset..]) {
                Ok(0) => return Err(err("OUTPUT_CLOSED")),
                Ok(n) => {
                    self.offset += n;
                    self.queued -= n;
                    if self.offset == bytes.len() {
                        self.out.pop_front();
                        self.offset = 0
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
    fn read(&mut self, r: &mut impl Read) -> io::Result<bool> {
        let mut chunk = [0; 65536];
        match r.read(&mut chunk) {
            Ok(0) => Ok(false),
            Ok(n) => {
                if self.input.len() + n > MAX_RECORD + 65536 {
                    return Err(err("INPUT_LIMIT"));
                }
                self.input.extend_from_slice(&chunk[..n]);
                Ok(true)
            }
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock
                    || e.kind() == io::ErrorKind::Interrupted =>
            {
                Ok(true)
            }
            Err(e) => Err(e),
        }
    }
    fn ipc(&mut self, t: u16, g: u64, p: &[u8]) -> io::Result<()> {
        let mut b = Vec::with_capacity(16 + p.len());
        b.extend_from_slice(&2u16.to_le_bytes());
        b.extend_from_slice(&t.to_le_bytes());
        b.extend_from_slice(&g.to_le_bytes());
        b.extend_from_slice(&(p.len() as u32).to_le_bytes());
        b.extend_from_slice(p);
        self.push(b)
    }
    fn record(&mut self) -> io::Result<Option<(u16, u64, Vec<u8>)>> {
        if self.input.len() < 16 {
            return Ok(None);
        }
        let b = &self.input;
        let v = u16::from_le_bytes(b[0..2].try_into().unwrap());
        let t = u16::from_le_bytes(b[2..4].try_into().unwrap());
        let g = u64::from_le_bytes(b[4..12].try_into().unwrap());
        let n = u32::from_le_bytes(b[12..16].try_into().unwrap()) as usize;
        if v != 2 || n > 65536 {
            return Err(err("IPC_HEADER"));
        }
        if b.len() < 16 + n {
            return Ok(None);
        }
        let p = b[16..16 + n].to_vec();
        self.input.drain(..16 + n);
        Ok(Some((t, g, p)))
    }
}
#[derive(PartialEq)]
enum Phase {
    Opening,
    Running,
    Draining,
}
#[derive(Default)]
struct MetadataPending {
    parts: [Option<Vec<u8>>; 3],
}
impl MetadataPending {
    fn merge(&mut self, flags: u32, lengths: &[u32], bytes: &[u8]) {
        if flags & 8 != 0 || lengths[1] > 0 {
            self.parts[0] = None;
            self.parts[2] = None;
        }
        let mut offset = 0;
        for i in 0..3 {
            let length = lengths[i] as usize;
            if flags & (4 << i) != 0 || length > 0 {
                let cap = if i < 2 { 65536 } else { LIMIT - 16 - 2 * 65536 };
                // A malformed/oversized optional field clears that field; audio survives.
                self.parts[i] = Some(if length <= cap {
                    bytes[offset..offset + length].to_vec()
                } else {
                    Vec::new()
                });
            }
            offset += length;
        }
    }
    fn packet(&self) -> Option<Vec<u8>> {
        if self.parts.iter().all(Option::is_none) {
            return None;
        }
        let flags = self
            .parts
            .iter()
            .enumerate()
            .fold(0u32, |f, (i, p)| f | if p.is_some() { 4 << i } else { 0 });
        let mut packet = Vec::new();
        packet.extend_from_slice(&flags.to_le_bytes());
        for p in &self.parts {
            packet.extend_from_slice(&(p.as_ref().map_or(0, Vec::len) as u32).to_le_bytes());
        }
        for p in self.parts.iter().flatten() {
            packet.extend_from_slice(p);
        }
        Some(packet)
    }
}
struct Stream {
    generation: u64,
    format: PcmFormat,
    descriptor: Vec<u8>,
    start: BTreeMap<String, String>,
    phase: Phase,
    network: u64,
    sent: u64,
    accepted: u64,
    rendered: u64,
    clock_seen: bool,
    audio: VecDeque<Vec<u8>>,
    pending_bytes: usize,
    replies: VecDeque<u64>,
    end: bool,
    drain_sent: bool,
    metadata: MetadataPending,
}
fn descriptor(f: PcmFormat, period: u32) -> Vec<u8> {
    let mut b = vec![0; 36];
    b[0..4].copy_from_slice(&f.rate.to_le_bytes());
    b[4..6].copy_from_slice(&f.channels.to_le_bytes());
    b[6..8].copy_from_slice(&f.valid_bits.to_le_bytes());
    b[8..10].copy_from_slice(&f.container_bits.to_le_bytes());
    b[10..12].copy_from_slice(&f.kind.to_le_bytes());
    b[12..16].copy_from_slice(&f.dsd_rate.to_le_bytes());
    b[16..20].copy_from_slice(&(f.bytes_per_frame().unwrap() as u32).to_le_bytes());
    b[32..36].copy_from_slice(&period.to_le_bytes());
    b
}
fn number(a: &BTreeMap<String, String>, key: &str) -> io::Result<u32> {
    a.get(key)
        .ok_or_else(|| err("ATTRIBUTE_MISSING"))?
        .parse()
        .map_err(|_| err("ATTRIBUTE_INTEGER"))
}
fn u64at(b: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(b[offset..offset + 8].try_into().unwrap())
}
fn dsd_output_bytes(input: &[u8]) -> io::Result<Vec<u8>> {
    if input.len() % 8 != 0 {
        return Err(err("DSD_ALIGNMENT"));
    }
    let mut out = Vec::with_capacity(input.len());
    for q in input.chunks_exact(8) {
        out.extend_from_slice(&[q[0], q[2], q[4], q[6], q[1], q[3], q[5], q[7]]);
    }
    Ok(out)
}
fn session(
    client: &mut TcpStream,
    ipc: &mut UnixStream,
    offers: &[Offer],
    name: &str,
    generation: &mut u64,
) -> io::Result<()> {
    let port = env::var("HIPHI_NAA_AUTH_PORT")
        .map_err(|_| err("AUTH_PORT"))?
        .parse()
        .map_err(|_| err("AUTH_PORT"))?;
    authenticate(client, port)?;
    client.set_nonblocking(true)?;
    ipc.set_nonblocking(true)?;
    let (mut net, mut local) = (Buffer::default(), Buffer::default());
    let mut stream: Option<Stream> = None;
    let mut initialized = false;
    let mut period_ms = 250u32;
    let mut init_channels = 2u16;
    let result = (|| -> io::Result<()> {
        loop {
            net.flush(client)?;
            local.flush(ipc)?;
            let mut fds = [
                libc::pollfd {
                    fd: client.as_raw_fd(),
                    events: libc::POLLIN | if net.queued > 0 { libc::POLLOUT } else { 0 },
                    revents: 0,
                },
                libc::pollfd {
                    fd: ipc.as_raw_fd(),
                    events: libc::POLLIN | if local.queued > 0 { libc::POLLOUT } else { 0 },
                    revents: 0,
                },
            ];
            if unsafe { libc::poll(fds.as_mut_ptr(), 2, 10) } < 0 {
                let e = io::Error::last_os_error();
                if e.kind() != io::ErrorKind::Interrupted {
                    return Err(e);
                }
            }
            if fds[0].revents & (libc::POLLIN | libc::POLLHUP) != 0 && !net.read(client)? {
                if !net.input.is_empty() {
                    return Err(err("TRUNCATED_EOF"));
                }
                return Ok(());
            }
            if fds[1].revents & (libc::POLLIN | libc::POLLHUP) != 0 && !local.read(ipc)? {
                return Err(err("IPC_EOF"));
            }
            while let Some((kind, g, p)) = local.record()? {
                let s = stream.as_mut().ok_or_else(|| err("IPC_WITHOUT_STREAM"))?;
                if g != s.generation {
                    return Err(err("IPC_STALE_GENERATION"));
                }
                match kind {
                    6 => {
                        if s.phase != Phase::Opening || p != s.descriptor {
                            return Err(err("IPC_GRANT_MISMATCH"));
                        }
                        s.phase = Phase::Running;
                        let mut a = s.start.clone();
                        a.insert(
                            "dsd".into(),
                            if s.format.kind == 3 { "1" } else { "0" }.into(),
                        );
                        net.push(response("start", &a, "", true))?;
                        net.push(vec![0; 16])?;
                    }
                    7 => {
                        if s.phase != Phase::Opening {
                            return Err(err("IPC_REFUSAL_STATE"));
                        }
                        net.push(response("start", &s.start, "", false))?;
                        stream = None;
                    }
                    9 => {
                        if p.len() != 8 {
                            return Err(err("IPC_ACCEPTED_LENGTH"));
                        }
                        let n = u64at(&p, 0);
                        if n < s.accepted || n > s.sent {
                            return Err(err("IPC_ACCEPTED_RANGE"));
                        }
                        s.accepted = n;
                    }
                    5 => {
                        if p.len() != 16 {
                            return Err(err("IPC_POSITION_LENGTH"));
                        }
                        let n = u64at(&p, 0);
                        if n < s.rendered || n > s.sent {
                            return Err(err("IPC_POSITION_RANGE"));
                        }
                        s.rendered = n;
                        s.clock_seen = true;
                    }
                    8 => {
                        if s.phase != Phase::Draining
                            || !s.drain_sent
                            || p.len() != 16
                            || u64at(&p, 0) != s.network
                            || u64at(&p, 8) != s.network
                        {
                            return Err(err("IPC_DRAIN_INCOMPLETE"));
                        }
                        while s.replies.pop_front().is_some() {
                            net.push(vec![0; 16])?
                        }
                        net.push(response("stop", &BTreeMap::new(), "", true))?;
                        stream = None;
                    }
                    _ => return Err(err("IPC_UNEXPECTED_TYPE")),
                }
            }
            loop {
                if net.input.is_empty() {
                    break;
                }
                if net.input[0] == b'<' {
                    let end = match net.input.iter().position(|&b| b == b'\n') {
                        Some(n) => n + 1,
                        None => {
                            if net.input.len() > 65536 {
                                return Err(err("CONTROL_LIMIT"));
                            }
                            break;
                        }
                    };
                    let raw: Vec<_> = net.input.drain(..end).collect();
                    let (kind, a) = operation(&raw)?;
                    match kind.as_str() {
                        "getdevices" => {
                            if a.get("direction").map(String::as_str) != Some("output") {
                                return Err(err("DIRECTION"));
                            }
                            net.push(response(
                                &kind,
                                &a,
                                &format!(
                                    "<device description=\"{}\" id=\"{}\"/>",
                                    escaped(name),
                                    DEVICE
                                ),
                                true,
                            ))?;
                        }
                        "initialize" => {
                            if stream.is_some()
                                || a.get("device").map(String::as_str) != Some(DEVICE)
                                || a.get("direction").map(String::as_str) != Some("output")
                                || number(&a, "channel_offset")? != 0
                                || number(&a, "pack_sdm")? != 0
                            {
                                return Err(err("INITIALIZE_UNSUPPORTED"));
                            }
                            let channels = number(&a, "channels")?;
                            if !(1..=8).contains(&channels) {
                                return Err(err("CHANNELS"));
                            }
                            init_channels = channels as u16;
                            if !offers.iter().any(|o| o.format.channels == init_channels) {
                                return Err(err("NO_CHANNEL_OFFERS"));
                            }
                            period_ms = number(&a, "periodtime")?;
                            if !(1..=2000).contains(&period_ms) {
                                return Err(err("PERIOD"));
                            }
                            initialized = true;
                            let mut reply = a;
                            for (k, v) in [
                                ("version", "6"),
                                ("metadata", "1"),
                                ("picture", "1"),
                                ("position", "1"),
                            ] {
                                reply.insert(k.into(), v.into());
                            }
                            net.push(response(&kind, &reply, "", true))?;
                        }
                        "getformats" => {
                            if !initialized {
                                return Err(err("NOT_INITIALIZED"));
                            }
                            let first = offers
                                .iter()
                                .filter(|o| o.format.channels == init_channels)
                                .max_by_key(|o| o.format.valid_bits)
                                .ok_or_else(|| err("NO_CHANNEL_OFFERS"))?
                                .format;
                            let mut body=format!("<volume_range min=\"0.00000000000000000\" max=\"0.00000000000000000\"/><initial_format bits=\"{}\" channels=\"{}\" dsd=\"0\" rate=\"0\"/>",first.valid_bits,init_channels);
                            // NAA enumerates one preferred exact width per rate, not
                            // every local PCM width as a separate controller rate.
                            let mut advertised = BTreeMap::new();
                            for o in offers.iter().filter(|o| o.format.channels == init_channels) {
                                let f = o.format;
                                let key =
                                    (f.kind == 3, if f.kind == 3 { f.dsd_rate } else { f.rate });
                                let preferred = advertised.entry(key).or_insert(f);
                                if f.valid_bits > preferred.valid_bits {
                                    *preferred = f;
                                }
                            }
                            for f in advertised.values().copied() {
                                if f.channels == init_channels {
                                    if f.kind == 3 {
                                        body.push_str(&format!("<format bits=\"1\" channels=\"{}\" dsd=\"1\" pcm=\"1\" sdm=\"1\" rate=\"{}\"/>",f.channels,f.dsd_rate));
                                    } else {
                                        body.push_str(&format!("<format bits=\"{}\" channels=\"{}\" dsd=\"0\" pcm=\"1\" sdm=\"0\" rate=\"{}\"/>",f.valid_bits,f.channels,f.rate));
                                    }
                                }
                            }
                            net.push(response(&kind, &a, &body, true))?;
                        }
                        "keepalive" => net.push(response(&kind, &a, "", true))?,
                        "start" => {
                            if !initialized || stream.is_some() {
                                return Err(err("START_STATE"));
                            }
                            let _rate = number(&a, "rate")?;
                            let bits = number(&a, "bits")?;
                            let channels = number(&a, "channels")?;
                            let requested_kind =
                                if a.get("stream").map(String::as_str) == Some("dsd") {
                                    3
                                } else {
                                    1
                                };
                            let requested_rate = number(&a, "rate")?;
                            let f = offers.iter().map(|o| o.format).find(|f| {
                                ((requested_kind == 3
                                    && f.kind == 3
                                    && f.dsd_rate == requested_rate
                                    && bits == 1)
                                    || (requested_kind != 3
                                        && f.kind != 3
                                        && f.rate == requested_rate
                                        && f.valid_bits as u32 == bits))
                                    && f.channels as u32 == channels
                                    && f.channels == init_channels
                            });
                            if f.is_none()
                                || (requested_kind == 3
                                    && a.get("bits").map(String::as_str) != Some("1"))
                                || (requested_kind == 1
                                    && a.get("stream").map(String::as_str) != Some("pcm"))
                            {
                                net.push(response(&kind, &a, "", false))?;
                                continue;
                            }
                            let format = f.unwrap();
                            *generation = generation
                                .checked_add(1)
                                .ok_or_else(|| err("GENERATION_OVERFLOW"))?;
                            let period = ((format.rate as u64 * period_ms as u64) / 1000)
                                .clamp(1, 1048576) as u32;
                            let desc = descriptor(format, period);
                            local.ipc(1, *generation, &desc)?;
                            stream = Some(Stream {
                                generation: *generation,
                                format,
                                descriptor: desc,
                                start: a,
                                phase: Phase::Opening,
                                network: 0,
                                sent: 0,
                                accepted: 0,
                                rendered: 0,
                                clock_seen: false,
                                audio: VecDeque::new(),
                                pending_bytes: 0,
                                replies: VecDeque::new(),
                                end: false,
                                drain_sent: false,
                                metadata: MetadataPending::default(),
                            });
                        }
                        "stop" => {
                            let s = stream.as_mut().ok_or_else(|| err("STOP_WITHOUT_STREAM"))?;
                            if s.phase != Phase::Running {
                                return Err(err("STOP_STATE"));
                            }
                            s.phase = Phase::Draining;
                        }
                        _ => return Err(err("UNSUPPORTED_OPERATION")),
                    }
                } else {
                    let s = stream.as_mut().ok_or_else(|| err("AUDIO_WITHOUT_STREAM"))?;
                    if s.phase != Phase::Running || s.end {
                        return Err(err("AUDIO_STATE"));
                    }
                    if net.input.len() < 32 {
                        break;
                    }
                    let fields: Vec<u32> = net.input[..32]
                        .chunks_exact(4)
                        .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
                        .collect();
                    // JPLAY first-record capture included opaque bit 24 with side sections.
                    // Lengths still delimit PCM; do not reinterpret that bit as audio.
                    if fields[0] & !0x0100_001f != 0
                        || fields[5..].iter().any(|&n| n != 0)
                        || (s.format.kind != 3 && fields[1] % s.format.channels as u32 != 0)
                    {
                        return Err(err("AUDIO_HEADER"));
                    }
                    let pcm = if s.format.kind == 3 {
                        let bpf = s.format.bytes_per_frame().map_err(|_| err("FRAME_SIZE"))?;
                        if fields[1] as usize % bpf != 0 {
                            return Err(err("DSD_ALIGNMENT"));
                        }
                        fields[1] as usize
                    } else {
                        (fields[1] as usize)
                            .checked_mul(s.format.container_bits as usize / 8)
                            .ok_or_else(|| err("AUDIO_OVERFLOW"))?
                    };
                    let mut n = 32usize
                        .checked_add(pcm)
                        .ok_or_else(|| err("AUDIO_OVERFLOW"))?;
                    for x in &fields[2..5] {
                        n = n
                            .checked_add(*x as usize)
                            .ok_or_else(|| err("AUDIO_OVERFLOW"))?
                    }
                    if pcm > LIMIT || n > MAX_RECORD {
                        return Err(err("AUDIO_LIMIT"));
                    }
                    if net.input.len() < n {
                        break;
                    }
                    if fields[0] == 1 && fields[1..].iter().all(|&x| x == 0) {
                        net.input.drain(..32);
                        s.end = true;
                        continue;
                    }
                    if s.replies.len() >= 8 || s.pending_bytes + pcm > 2 * LIMIT {
                        return Err(err("AUDIO_BACKLOG"));
                    }
                    let bpf = s.format.bytes_per_frame().map_err(|_| err("FRAME_SIZE"))?;
                    let chunk = 65536 - 65536 % bpf;
                    for bytes in net.input[32..32 + pcm].chunks(chunk) {
                        s.audio.push_back(if s.format.kind == 3 {
                            dsd_output_bytes(bytes)?
                        } else {
                            bytes.to_vec()
                        });
                    }
                    s.metadata
                        .merge(fields[0], &fields[2..5], &net.input[32 + pcm..n]);
                    s.pending_bytes += pcm;
                    s.network = s
                        .network
                        .checked_add((pcm / bpf) as u64)
                        .ok_or_else(|| err("FRAME_OVERFLOW"))?;
                    s.replies.push_back(s.network);
                    net.input.drain(..n);
                }
            }
            if let Some(s) = stream.as_mut() {
                let bpf = s.format.bytes_per_frame().map_err(|_| err("FRAME_SIZE"))?;
                while let Some(bytes) = s.audio.front() {
                    if (s.sent - s.accepted) * bpf as u64 + bytes.len() as u64 > 262144 {
                        break;
                    }
                    let bytes = s.audio.pop_front().unwrap();
                    s.pending_bytes -= bytes.len();
                    local.ipc(2, s.generation, &bytes)?;
                    s.sent = s
                        .sent
                        .checked_add((bytes.len() / bpf) as u64)
                        .ok_or_else(|| err("FRAME_OVERFLOW"))?;
                }
                if let Some(packet) = s.metadata.packet() {
                    if local.queued + packet.len() + 16 <= 2 * LIMIT {
                        local.ipc(10, s.generation, &packet)?;
                        s.metadata = MetadataPending::default();
                    }
                }
                if s.phase == Phase::Draining && !s.drain_sent && s.audio.is_empty() {
                    local.ipc(3, s.generation, &[])?;
                    s.drain_sent = true;
                }
                if s.clock_seen && s.network - s.rendered <= s.format.rate as u64 {
                    while let Some(&target) = s.replies.front() {
                        if target > s.accepted {
                            break;
                        }
                        s.replies.pop_front();
                        let seconds = (s.network - s.rendered) as f32 / s.format.rate as f32;
                        let mut reply = vec![0; 16];
                        reply[4..8].copy_from_slice(&seconds.to_le_bytes());
                        net.push(reply)?;
                    }
                }
            }
        }
    })();
    if let Some(s) = stream {
        let _ = local.ipc(4, s.generation, &[]);
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            local.flush(ipc)?;
            if !local.read(ipc)? {
                return Err(err("IPC_ABORT_EOF"));
            }
            while let Some((t, g, _)) = local.record()? {
                if t == 8 && g == s.generation {
                    return result;
                }
            }
            if Instant::now() >= deadline {
                return Err(err("IPC_ABORT_TIMEOUT"));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    result
}
pub fn serve(listener: TcpListener, offers: Vec<Offer>, device_name: &str) -> io::Result<()> {
    let name = env::var("HIPHI_NAA_IPC_SOCKET").map_err(|_| err("IPC_SOCKET_UNSET"))?;
    let mut ipc = connect_ipc(&name)?;
    let mut generation = 0;
    for incoming in listener.incoming() {
        let mut client = incoming?;
        let result = session(&mut client, &mut ipc, &offers, device_name, &mut generation);
        client.shutdown(Shutdown::Both).ok();
        if let Err(e) = result {
            eprintln!("NAA session: {e}");
            if e.to_string().starts_with("IPC_") {
                return Err(e);
            }
        }
    }
    Ok(())
}
pub fn configured_offers() -> io::Result<Vec<Offer>> {
    let text = env::var("HIPHI_NAA_FORMATS").map_err(|_| err("FORMAT_OFFERS_MISSING"))?;
    if text.len() > 65536 {
        return Err(err("FORMAT_OFFERS_LIMIT"));
    }
    let mut offers = Vec::new();
    for row in text.split(',') {
        let f: Vec<u32> = row
            .split(':')
            .map(|s| s.parse().map_err(|_| err("FORMAT_OFFER")))
            .collect::<io::Result<_>>()?;
        if f.len() != 6 || f[1] > u16::MAX as u32 || f[2] > u16::MAX as u32 || f[3] > 8 {
            return Err(err("FORMAT_OFFER"));
        }
        let endian = if f[4] == 3 {
            crate::pcm::Endian::Big
        } else {
            crate::pcm::Endian::Little
        };
        let format = PcmFormat::new_descriptor(
            f[0],
            f[1] as u16,
            f[2] as u16,
            f[3] as u16,
            endian,
            f[4] as u16,
            f[5],
        )
        .map_err(|_| err("FORMAT_OFFER"))?;
        // No reviewed NAA wire mapping exists yet for DoP or unequal
        // valid/container PCM widths; keep those route capabilities out of
        // this adapter until their exact payload geometry is evidenced.
        if format.kind == 2 || (format.kind == 1 && format.valid_bits != format.container_bits) {
            continue;
        }
        if !offers.iter().any(|o: &Offer| o.format == format) {
            offers.push(Offer::exact(format))
        }
    }
    if offers.is_empty() {
        return Err(err("NO_UNAMBIGUOUS_PCM_OFFERS"));
    }
    Ok(offers)
}
pub fn run() -> io::Result<()> {
    let port: u16 = env::var("HIPHI_NAA_PORT")
        .unwrap_or_else(|_| "43210".into())
        .parse()
        .map_err(|_| err("PUBLIC_PORT"))?;
    let bind = env::var("HIPHI_NAA_BIND").unwrap_or_else(|_| "0.0.0.0".into());
    let name = env::var("HIPHI_NAA_DEVICE_NAME").unwrap_or_else(|_| "HiPhi direct output".into());
    let offers = configured_offers()?;
    let listener = TcpListener::bind((bind.as_str(), port))?;
    if env::var("HIPHI_NAA_DISCOVERY").as_deref() != Ok("0") {
        let udp = crate::discovery::bind(port)?;
        let identity = env::var("HIPHI_NAA_NAME").unwrap_or_else(|_| "HiPhi Endpoints".into());
        crate::discovery::response(&identity)?;
        std::thread::spawn(move || {
            if let Err(e) = crate::discovery::serve(udp, identity) {
                eprintln!("discovery: {e}");
                std::process::exit(1)
            }
        });
    }
    serve(listener, offers, &name)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn metadata_coalesces_sections_and_new_track_resets_pending_state() {
        let mut pending = MetadataPending::default();
        pending.merge(28, &[3, 3, 3], b"posoldart");
        pending.merge(4, &[3, 0, 0], b"new");
        assert_eq!(pending.parts[0].as_deref(), Some(&b"new"[..]));
        assert_eq!(pending.parts[1].as_deref(), Some(&b"old"[..]));
        assert_eq!(pending.parts[2].as_deref(), Some(&b"art"[..]));
        pending.merge(12, &[3, 3, 0], b"nowtag");
        assert_eq!(pending.parts[0].as_deref(), Some(&b"now"[..]));
        assert_eq!(pending.parts[1].as_deref(), Some(&b"tag"[..]));
        assert!(pending.parts[2].is_none());
        pending.merge(8, &[0, 0, 0], b"");
        assert!(pending.parts[0].is_none());
        assert_eq!(pending.parts[1].as_deref(), Some(&b""[..]));
    }
    #[test]
    fn descriptor_is_exact() {
        let f = PcmFormat::new(123457, 24, 24, 6).unwrap();
        let b = descriptor(f, 1024);
        assert_eq!(b.len(), 36);
        assert_eq!(&b[20..32], &[0; 12]);
        assert_eq!(u32::from_le_bytes(b[32..].try_into().unwrap()), 1024)
    }
    #[test]
    fn dsd_wire_permutation_is_lossless_and_asymmetric() {
        let wire = [0, 1, 2, 3, 4, 5, 6, 7];
        assert_eq!(
            dsd_output_bytes(&wire).unwrap(),
            vec![0, 2, 4, 6, 1, 3, 5, 7]
        );
        let out = dsd_output_bytes(&wire).unwrap();
        let mut inverse = [0u8; 8];
        for (i, &p) in [0, 2, 4, 6, 1, 3, 5, 7].iter().enumerate() {
            inverse[p] = out[i];
        }
        assert_eq!(inverse, wire);
    }
    #[test]
    fn dsd256_descriptor_uses_carrier_and_exact_bitrate() {
        let f = PcmFormat::new_descriptor(352800, 32, 32, 2, crate::pcm::Endian::Big, 3, 11289600)
            .unwrap();
        let d = descriptor(f, 88200);
        assert_eq!(u32::from_le_bytes(d[0..4].try_into().unwrap()), 352800);
        assert_eq!(u16::from_le_bytes(d[10..12].try_into().unwrap()), 3);
        assert_eq!(u32::from_le_bytes(d[12..16].try_into().unwrap()), 11289600);
        assert_eq!(u32::from_le_bytes(d[16..20].try_into().unwrap()), 8);
    }
    #[test]
    fn large_dsd_permutation_remains_frame_exact() {
        let wire = (0..(65536 + 8)).map(|x| x as u8).collect::<Vec<_>>();
        let out = dsd_output_bytes(&wire).unwrap();
        assert_eq!(out.len(), wire.len());
        for (i, q) in wire.chunks_exact(8).enumerate() {
            assert_eq!(
                &out[i * 8..i * 8 + 8],
                &[q[0], q[2], q[4], q[6], q[1], q[3], q[5], q[7]]
            );
        }
    }
    #[test]
    fn control_is_structural() {
        assert!(operation(b"<?xml version=\"1.0\"?><networkaudio><operation type=\"start\" rate=\"123457\"/></networkaudio>\n").is_ok());
        assert!(operation(b"<networkaudio><x type=\"start\"/></networkaudio>").is_err());
        assert!(operation(b"<!DOCTYPE foo><networkaudio/>").is_err())
    }
}
