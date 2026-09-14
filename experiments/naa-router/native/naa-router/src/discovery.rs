//! Destination discovery is independent of the routing session and auth sockets.
use serde::Serialize;
use std::{
    collections::BTreeMap,
    io,
    net::{Ipv4Addr, SocketAddr, UdpSocket},
    sync::Mutex,
    time::{Duration, Instant},
};

const QUERY: &[u8] =
    b"<networkaudio><discover version=\"HiPhi Router\">network audio</discover></networkaudio>\0";
#[derive(Debug, Clone, Serialize)]
pub struct Endpoint {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub protocol: String,
}
pub struct Discovery {
    pub interface: Option<Ipv4Addr>,
    listener: SocketAddr,
    scan: Mutex<()>,
}
impl Discovery {
    pub fn new(interface: Option<Ipv4Addr>, listener: SocketAddr) -> Self {
        Self {
            interface,
            listener: if listener.ip().is_unspecified() {
                interface
                    .map(|ip| SocketAddr::new(ip.into(), listener.port()))
                    .unwrap_or(listener)
            } else {
                listener
            },
            scan: Mutex::new(()),
        }
    }
    pub fn scan(&self) -> Result<Vec<Endpoint>, String> {
        let _guard = self
            .scan
            .try_lock()
            .map_err(|_| "Discovery is already running")?;
        let ip = self
            .interface
            .ok_or("Discovery needs --discovery-interface; manual destinations remain available")?;
        let socket = UdpSocket::bind((ip, 0)).map_err(|e| e.to_string())?;
        set_interface(&socket, ip).map_err(|e| e.to_string())?;
        socket.set_multicast_ttl_v4(1).map_err(|e| e.to_string())?;
        for group in naa_native::discovery::GROUPS {
            socket
                .send_to(QUERY, (group, 43210))
                .map_err(|e| e.to_string())?;
        }
        collect(&socket, self.listener, Duration::from_secs(2)).map_err(|e| e.to_string())
    }
}
fn set_interface(socket: &UdpSocket, ip: Ipv4Addr) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let addr = libc::in_addr {
        s_addr: u32::from_ne_bytes(ip.octets()),
    };
    // in_addr is the platform's IP_MULTICAST_IF value; no pointer escapes this call.
    let result = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::IPPROTO_IP,
            libc::IP_MULTICAST_IF,
            &addr as *const _ as *const libc::c_void,
            std::mem::size_of_val(&addr) as libc::socklen_t,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
fn parse(packet: &[u8], peer: SocketAddr, listener: SocketAddr) -> Option<Endpoint> {
    if packet.len() > 4096
        || peer.port() == 0
        || peer.ip().is_multicast()
        || peer.ip().is_unspecified()
        || (peer.port() == listener.port()
            && (listener.ip().is_unspecified() || peer.ip() == listener.ip()))
    {
        return None;
    }
    let text = std::str::from_utf8(packet.strip_suffix(&[0]).unwrap_or(packet)).ok()?;
    if text.contains("<!") {
        return None;
    }
    let doc = roxmltree::Document::parse(text).ok()?;
    let root = doc.root_element();
    let mut children = root.children().filter(|n| n.is_element());
    let node = children.next()?;
    if root.tag_name().name() != "networkaudio"
        || children.next().is_some()
        || node.tag_name().name() != "discover"
        || node.text()? != "network audio"
        || node.attribute("result")? != "OK"
    {
        return None;
    }
    // Exclude other HiPhi routers too: never form a chain that can loop back.
    if node.attribute("os") == Some("HiPhi Router")
        || node
            .attribute("version")
            .is_some_and(|v| v.starts_with("HiPhi Router"))
    {
        return None;
    }
    let name = node.attribute("name")?;
    let protocol = node.attribute("protocol")?;
    if name.is_empty()
        || name.len() > 128
        || name.chars().any(char::is_control)
        || protocol.len() > 16
        || protocol.is_empty()
        || !protocol.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    Some(Endpoint {
        name: name.into(),
        host: peer.ip().to_string(),
        port: peer.port(),
        protocol: protocol.into(),
    })
}
fn collect(
    socket: &UdpSocket,
    listener: SocketAddr,
    budget: Duration,
) -> io::Result<Vec<Endpoint>> {
    let deadline = Instant::now() + budget;
    let mut found = BTreeMap::new();
    let mut buf = [0; 4097];
    while let Some(left) = deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
    {
        socket.set_read_timeout(Some(left))?;
        match socket.recv_from(&mut buf) {
            Ok((n, peer)) => {
                if let Some(endpoint) = parse(&buf[..n], peer, listener) {
                    if found.len() < 256 || found.contains_key(&peer) {
                        found.insert(peer, endpoint);
                    }
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                break
            }
            Err(e) => return Err(e),
        }
    }
    Ok(found.into_values().collect())
}
#[cfg(test)]
mod tests {
    use super::*;
    fn reply(name: &str) -> Vec<u8> {
        naa_native::discovery::response(name).unwrap()
    }
    #[test]
    fn rejects_self_router_chains_requests_and_malformed_packets() {
        let peer = "127.0.0.1:43210".parse().unwrap();
        let other = "127.0.0.2:43210".parse().unwrap();
        assert!(parse(&reply("room"), peer, peer).is_none());
        assert!(parse(QUERY, peer, other).is_none());
        assert!(parse(&reply("room"), peer, other).is_some());
        let proxy = String::from_utf8(reply("room"))
            .unwrap()
            .replace("android", "HiPhi Router");
        assert!(parse(proxy.as_bytes(), peer, other).is_none());
        assert!(parse(b"<!DOCTYPE x><networkaudio/>", peer, other).is_none());
        assert!(parse(&vec![b'x'; 4097], peer, other).is_none());
        assert!(parse(&reply(&"x".repeat(129)), peer, other).is_none());
    }
    #[test]
    fn duplicate_names_remain_distinct_and_empty_next_scan_expires_results() {
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        let a = UdpSocket::bind("127.0.0.1:0").unwrap();
        let b = UdpSocket::bind("127.0.0.1:0").unwrap();
        for sender in [&a, &a, &b] {
            sender
                .send_to(&reply("same"), receiver.local_addr().unwrap())
                .unwrap();
        }
        let listener = "127.0.0.1:1".parse().unwrap();
        let result = collect(&receiver, listener, Duration::from_millis(40)).unwrap();
        assert_eq!(result.len(), 2);
        assert_ne!(result[0].port, result[1].port);
        assert!(collect(&receiver, listener, Duration::from_millis(40))
            .unwrap()
            .is_empty());
    }
}
