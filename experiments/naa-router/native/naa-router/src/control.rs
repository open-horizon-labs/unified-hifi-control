//! Optional HQPlayer native control client, transport commands only.
//!
//! Element names and attributes follow tools/hqp_control.py in this repository,
//! which was independently implemented from the locally inspected HQPlayer
//! ControlInterface source (HQPlayer Control, Copyright (C) 2011-2026 Jussi
//! Laako, MIT license). This is HQPlayer's control API, not NAA. Only `State`,
//! `Status`, `Stop` and `Play` are issued here; configuration, profile, restart,
//! reset and track-selection commands are deliberately absent.
//!
//! Framing: HQPlayer answers with an XML declaration plus one root element, not
//! necessarily newline-terminated. The reply is accumulated and re-parsed until
//! it forms a complete document, bounded by size and an absolute deadline.
use std::{
    collections::BTreeMap,
    io::{self, Read, Write},
    net::{SocketAddr, TcpStream},
    time::{Duration, Instant},
};
const REPLY_LIMIT: usize = 65536;
#[derive(Debug, Clone)]
pub struct Reply {
    pub tag: String,
    pub attrs: BTreeMap<String, String>,
    /// Trimmed root text, where HQPlayer puts refusal reasons such as
    /// "...not seekable!".
    pub text: String,
}
impl Reply {
    pub fn attr(&self, key: &str) -> Option<&str> {
        self.attrs.get(key).map(String::as_str)
    }
}
fn remaining(deadline: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "control deadline expired"))
}
fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
/// Issue one command and return its reply. `register` receives the connected
/// socket so the owner can shut it down when the operation is superseded or
/// stopped; returning `false` aborts before anything is sent.
pub fn request(
    addr: SocketAddr,
    command: &str,
    attrs: &[(&str, &str)],
    deadline: Instant,
    register: &mut dyn FnMut(&TcpStream) -> bool,
) -> io::Result<Reply> {
    let mut stream = TcpStream::connect_timeout(&addr, remaining(deadline)?)?;
    stream.set_nodelay(true)?;
    if !register(&stream) {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "operation superseded",
        ));
    }
    let mut payload = format!("<?xml version=\"1.0\"?><{command}");
    for (k, v) in attrs {
        payload.push_str(&format!(" {k}=\"{}\"", escape(v)));
    }
    payload.push_str("/>\n");
    stream.set_write_timeout(Some(remaining(deadline)?))?;
    stream.write_all(payload.as_bytes())?;
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        stream.set_read_timeout(Some(remaining(deadline)?))?;
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "HQPlayer closed the control connection without a complete reply",
            ));
        }
        buffer.extend_from_slice(&chunk[..n]);
        if buffer.len() > REPLY_LIMIT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "HQPlayer reply exceeds 64 KiB",
            ));
        }
        if let Some(reply) = parse(&buffer)? {
            if reply.tag != command {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("HQPlayer answered <{}> to <{command}>", reply.tag),
                ));
            }
            if let Some(result) = reply.attr("result") {
                if result != "OK" {
                    return Err(io::Error::other(format!(
                        "HQPlayer rejected <{command}>: result={result} {}",
                        reply.text
                    )));
                }
            }
            return Ok(reply);
        }
    }
}
/// `Ok(None)` while the buffer is still an incomplete document.
fn parse(buffer: &[u8]) -> io::Result<Option<Reply>> {
    let Ok(text) = std::str::from_utf8(buffer) else {
        // A multi-byte sequence may be split across reads; wait for more.
        return Ok(None);
    };
    if text.contains("<!") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "HQPlayer reply contains a declaration construct",
        ));
    }
    let Ok(doc) = roxmltree::Document::parse(text.trim()) else {
        return Ok(None);
    };
    let root = doc.root_element();
    Ok(Some(Reply {
        tag: root.tag_name().name().to_string(),
        attrs: root
            .attributes()
            .map(|a| (a.name().to_string(), a.value().to_string()))
            .collect(),
        text: root.text().unwrap_or("").trim().to_string(),
    }))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn incremental_reply_completes_only_as_a_full_document() {
        let full = b"<?xml version=\"1.0\"?><State state=\"2\" track=\"1\"/>";
        for cut in 1..full.len() {
            assert!(parse(&full[..cut]).unwrap().is_none(), "cut at {cut}");
        }
        let reply = parse(full).unwrap().unwrap();
        assert_eq!(reply.tag, "State");
        assert_eq!(reply.attr("state"), Some("2"));
        let trailing = parse(b"<?xml version=\"1.0\"?><Stop result=\"OK\"/>\n\n")
            .unwrap()
            .unwrap();
        assert_eq!(trailing.attr("result"), Some("OK"));
        assert!(parse(b"<!DOCTYPE x><State/>").is_err());
        let refused = parse(b"<Seek result=\"Error\">Source not seekable!</Seek>")
            .unwrap()
            .unwrap();
        assert_eq!(refused.attr("result"), Some("Error"));
        assert_eq!(refused.text, "Source not seekable!");
    }
}
