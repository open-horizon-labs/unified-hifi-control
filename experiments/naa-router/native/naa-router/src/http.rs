use crate::state::{NewRoute, Router};
use serde::Deserialize;
use std::{
    io::{self, BufRead, BufReader, Read, Write},
    net::{IpAddr, SocketAddr, TcpListener, TcpStream},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};
const BODY_LIMIT: usize = 65536;
pub fn serve(
    listener: TcpListener,
    router: Arc<Router>,
    discovery: Arc<crate::discovery::Discovery>,
) {
    let active = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming().flatten() {
        if active.fetch_add(1, Ordering::AcqRel) >= 32 {
            active.fetch_sub(1, Ordering::AcqRel);
            continue;
        }
        let r = router.clone();
        let a = active.clone();
        let discovery = discovery.clone();
        thread::spawn(move || {
            let _ = handle(stream, r, discovery);
            a.fetch_sub(1, Ordering::AcqRel);
        });
    }
}
fn respond(stream: &mut TcpStream, status: u16, kind: &str, body: &[u8]) -> io::Result<()> {
    let reason = match status {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        _ => "Internal Server Error",
    };
    write!(stream,"HTTP/1.1 {status} {reason}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nContent-Security-Policy: default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'\r\n\r\n",body.len())?;
    stream.write_all(body)
}
fn error(s: &mut TcpStream, status: u16, e: impl ToString) -> io::Result<()> {
    respond(
        s,
        status,
        "application/json",
        &serde_json::to_vec(&serde_json::json!({"error":e.to_string()}))?,
    )
}
/// Whole-request deadline. Per-read timeouts alone let a peer trickle one byte
/// every few seconds and hold one of the 32 worker slots indefinitely.
const REQUEST_DEADLINE: Duration = Duration::from_secs(10);
fn arm(r: &BufReader<TcpStream>, deadline: Instant) -> io::Result<()> {
    let left = deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "request deadline"))?;
    r.get_ref()
        .set_read_timeout(Some(left.min(Duration::from_secs(5))))
}
/// Read one CRLF line, re-arming the deadline before every underlying socket
/// read so a peer trickling one byte at a time cannot outlive it.
fn bounded_line(r: &mut BufReader<TcpStream>, deadline: Instant) -> io::Result<String> {
    let mut bytes = vec![];
    loop {
        arm(r, deadline)?;
        let buf = r.fill_buf()?;
        if buf.is_empty() {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        let (chunk, done) = match buf.iter().position(|&b| b == b'\n') {
            Some(i) => (&buf[..=i], true),
            None => (buf, false),
        };
        let n = chunk.len();
        if bytes.len() + n > 8192 {
            return Err(io::Error::other("invalid HTTP header"));
        }
        bytes.extend_from_slice(chunk);
        r.consume(n);
        if done {
            break;
        }
    }
    if !bytes.ends_with(b"\r\n") {
        return Err(io::Error::other("invalid HTTP header"));
    }
    String::from_utf8(bytes).map_err(io::Error::other)
}
fn bounded_body(
    r: &mut BufReader<TcpStream>,
    length: usize,
    deadline: Instant,
) -> io::Result<Vec<u8>> {
    let mut body = vec![0; length];
    let mut filled = 0;
    while filled < length {
        arm(r, deadline)?;
        let n = r.read(&mut body[filled..])?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        filled += n;
    }
    Ok(body)
}
/// Host must name this listener by IP literal (or localhost when bound to
/// loopback) with the exact port. DNS names are refused, which blocks DNS
/// rebinding; on an unspecified bind any literal interface address is accepted
/// because the exact interface a browser used is not known here.
fn host_allowed(host: &str, addr: SocketAddr) -> bool {
    let (name, port) = if let Some(rest) = host.strip_prefix('[') {
        let Some((ip, port)) = rest.split_once(']') else {
            return false;
        };
        (
            ip,
            port.strip_prefix(':')
                .or(if port.is_empty() { Some("") } else { None }),
        )
    } else {
        match host.rsplit_once(':') {
            Some((n, p)) => (n, Some(p)),
            None => (host, Some("")),
        }
    };
    let port_ok = match port {
        Some("") => addr.port() == 80,
        Some(p) => p.parse::<u16>().ok() == Some(addr.port()),
        None => false,
    };
    if !port_ok {
        return false;
    }
    if name.eq_ignore_ascii_case("localhost") {
        return addr.ip().is_loopback();
    }
    let Ok(ip) = name.parse::<IpAddr>() else {
        return false;
    };
    if addr.ip().is_unspecified() {
        return !ip.is_unspecified() && !ip.is_multicast();
    }
    if addr.ip().is_loopback() {
        return ip.is_loopback();
    }
    ip == addr.ip()
}
fn handle(
    stream: TcpStream,
    router: Arc<Router>,
    discovery: Arc<crate::discovery::Discovery>,
) -> io::Result<()> {
    let deadline = Instant::now() + REQUEST_DEADLINE;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(stream);
    let request = bounded_line(&mut reader, deadline)?;
    let words: Vec<_> = request.split_whitespace().collect();
    if words.len() != 3 || words[2] != "HTTP/1.1" {
        return error(reader.get_mut(), 400, "HTTP/1.1 required");
    }
    let (method, path) = (words[0], words[1]);
    let mut headers = std::collections::HashMap::new();
    let mut header_bytes = request.len();
    loop {
        let line = bounded_line(&mut reader, deadline)?;
        header_bytes += line.len();
        if header_bytes > 16384 {
            return error(reader.get_mut(), 413, "headers too large");
        }
        if line == "\r\n" {
            break;
        }
        let Some((key, value)) = line.trim_end().split_once(':') else {
            return error(reader.get_mut(), 400, "invalid header");
        };
        let key = key.to_ascii_lowercase();
        if headers.insert(key, value.trim().to_string()).is_some() {
            return error(reader.get_mut(), 400, "duplicate header");
        }
    }
    let host = headers.get("host").map(String::as_str).unwrap_or("");
    if !host_allowed(host, router.control_addr) {
        return error(
            reader.get_mut(),
            403,
            "unrecognized Host; use the printed control address",
        );
    }
    if let Some(origin) = headers.get("origin") {
        if origin != &format!("http://{host}") {
            return error(reader.get_mut(), 403, "cross-origin request refused");
        }
    }
    if headers
        .get("sec-fetch-site")
        .is_some_and(|v| v == "cross-site")
    {
        return error(reader.get_mut(), 403, "cross-site request refused");
    }
    if method == "GET" {
        let value = match path {
            "/" => {
                return respond(
                    reader.get_mut(),
                    200,
                    "text/html; charset=utf-8",
                    include_bytes!("../web/index.html"),
                )
            }
            "/api/state" => router.state(),
            "/api/discovery" => serde_json::json!({"enabled": discovery.interface.is_some()}),
            "/api/routes" => serde_json::to_value(&router.inner.lock().unwrap().config.routes)?,
            _ => return error(reader.get_mut(), 404, "not found"),
        };
        return respond(
            reader.get_mut(),
            200,
            "application/json",
            &serde_json::to_vec(&value)?,
        );
    }
    if method != "POST" {
        return error(reader.get_mut(), 405, "method not allowed");
    }
    if headers.contains_key("transfer-encoding") {
        return error(reader.get_mut(), 400, "chunked requests unsupported");
    }
    if headers
        .get("content-type")
        .map(|s| s.split(';').next().unwrap_or("").trim())
        != Some("application/json")
    {
        return error(
            reader.get_mut(),
            400,
            "Content-Type application/json required",
        );
    }
    let length = match headers
        .get("content-length")
        .and_then(|s| s.parse::<usize>().ok())
    {
        Some(n) if n <= BODY_LIMIT => n,
        Some(_) => return error(reader.get_mut(), 413, "body exceeds 64 KiB"),
        None => return error(reader.get_mut(), 400, "Content-Length required"),
    };
    let body = bounded_body(&mut reader, length, deadline)?;
    #[derive(Deserialize)]
    struct Select {
        route_id: String,
    }
    #[derive(Deserialize)]
    struct Update {
        route_id: String,
        #[serde(flatten)]
        route: NewRoute,
    }
    let result: Result<serde_json::Value, String> = match path {
        "/api/discover" => serde_json::from_slice::<serde_json::Value>(&body)
            .map_err(|e| e.to_string())
            .and_then(|_| discovery.scan())
            .and_then(|v| serde_json::to_value(v).map_err(|e| e.to_string())),
        "/api/routes" => serde_json::from_slice::<NewRoute>(&body)
            .map_err(|e| e.to_string())
            .and_then(|r| router.add(r))
            .and_then(|r| serde_json::to_value(r).map_err(|e| e.to_string())),
        "/api/routes/update" => serde_json::from_slice::<Update>(&body)
            .map_err(|e| e.to_string())
            .and_then(|v| router.edit(&v.route_id, v.route))
            .and_then(|v| serde_json::to_value(v).map_err(|e| e.to_string())),
        "/api/routes/remove" => serde_json::from_slice::<Select>(&body)
            .map_err(|e| e.to_string())
            .and_then(|v| router.remove(&v.route_id)),
        "/api/select" => serde_json::from_slice::<Select>(&body)
            .map_err(|e| e.to_string())
            .and_then(|s| router.select(&s.route_id)),
        "/api/stop" => serde_json::from_slice::<serde_json::Value>(&body)
            .map_err(|e| e.to_string())
            .and_then(|_| router.stop()),
        _ => return error(reader.get_mut(), 404, "not found"),
    };
    match result {
        Ok(value) => respond(
            reader.get_mut(),
            if path == "/api/routes" { 201 } else { 200 },
            "application/json",
            &serde_json::to_vec(&value)?,
        ),
        Err(e) => error(reader.get_mut(), 400, e),
    }
}
#[cfg(test)]
mod tests {
    use super::host_allowed;
    fn addr(s: &str) -> std::net::SocketAddr {
        s.parse().unwrap()
    }
    #[test]
    fn host_guard_matches_listener_by_literal_and_port() {
        let lo = addr("127.0.0.1:8787");
        assert!(host_allowed("127.0.0.1:8787", lo));
        assert!(host_allowed("localhost:8787", lo));
        assert!(host_allowed("[::1]:8787", lo));
        assert!(!host_allowed("127.0.0.1:8788", lo));
        assert!(!host_allowed("192.168.1.5:8787", lo));
        assert!(!host_allowed("evil.example:8787", lo));
        assert!(!host_allowed("", lo));
        let lan = addr("192.168.1.5:8787");
        assert!(host_allowed("192.168.1.5:8787", lan));
        assert!(!host_allowed("localhost:8787", lan));
        assert!(!host_allowed("router.lan:8787", lan));
        // Unspecified bind: the interface used is unknown, so any IP literal on
        // the exact port passes; DNS names are still refused.
        let any = addr("0.0.0.0:8787");
        assert!(host_allowed("192.168.1.5:8787", any));
        assert!(host_allowed("[fe80::1]:8787", any));
        assert!(!host_allowed("0.0.0.0:8787", any));
        assert!(!host_allowed("router.lan:8787", any));
        assert!(!host_allowed("192.168.1.5", any));
    }
}
