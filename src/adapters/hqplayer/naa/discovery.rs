//! NAA host discovery: native NAA UDP multicast, not mDNS.
//!
//! Ported from the private PoC (`experiments/naa-router/native/naa-router/src/discovery.rs`,
//! kept verbatim there). Uses a separate socket on an explicit interface, never authenticates,
//! never starts playback and never changes a route. Results are observations of who answered; a
//! host that answered is not thereby known to be attached to any DAC, which is why this module
//! produces [`HqpDiscoveryObservation`] (hosts) and never a DAC list.

use std::collections::BTreeMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::outputs::{HqpDiscoveredEndpoint, HqpDiscoveryObservation};

/// Multicast groups the reference endpoint listens on (observed 2026-09-13, PoC discovery.rs).
pub const GROUPS: [Ipv4Addr; 2] = [
    Ipv4Addr::new(224, 0, 0, 199),
    Ipv4Addr::new(239, 192, 0, 199),
];
const QUERY: &[u8] =
    b"<networkaudio><discover version=\"HiPhi Router\">network audio</discover></networkaudio>\0";
const SCAN_BUDGET: Duration = Duration::from_secs(2);
const MAX_RESULTS: usize = 256;

/// Whether a packet is a valid NAA discovery request (`<networkaudio><discover …>network
/// audio</discover></networkaudio>`), as the reference endpoint validates it.
pub fn valid_request(packet: &[u8]) -> bool {
    if packet.len() > 4096 {
        return false;
    }
    let raw = packet.strip_suffix(&[0]).unwrap_or(packet);
    let Ok(text) = std::str::from_utf8(raw) else {
        return false;
    };
    if text.contains("<!") {
        return false;
    }
    let Ok(doc) = roxmltree::Document::parse(text) else {
        return false;
    };
    let root = doc.root_element();
    let nodes: Vec<_> = root.children().filter(|n| n.is_element()).collect();
    root.tag_name().name() == "networkaudio"
        && nodes.len() == 1
        && nodes[0].tag_name().name() == "discover"
        && nodes[0].text() == Some("network audio")
        // A reply carries `result`; a request never does. Without this, two responders on one
        // host would answer each other's advertisements forever.
        && nodes[0].attribute("result").is_none()
}

/// The advertisement the managed relay answers with: the stable adapter name, `os="HiPhi Router"`
/// (which is also how other relays recognise and exclude it), protocol 6. XML-escaped.
pub fn advertisement(name: &str) -> Result<Vec<u8>, String> {
    if name.is_empty() || name.len() > 256 || name.chars().any(char::is_control) {
        return Err("adapter name must contain 1–256 printable bytes".into());
    }
    let escaped = name
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?><networkaudio><discover name=\"{escaped}\" os=\"HiPhi Router\" protocol=\"6\" result=\"OK\" trigger=\"1\" version=\"HiPhi Router UHC 0.1\">network audio</discover></networkaudio>\0"
    )
    .into_bytes())
}

/// The concrete socket addresses a relay listener answers on, for self-exclusion. A wildcard bind
/// is expanded to every local IPv4 address on the listener's port; nothing else on that port is
/// ever excluded, so remote NAAs on the standard port stay discoverable.
pub fn own_addresses(listener: Option<SocketAddr>) -> Vec<SocketAddr> {
    let Some(listener) = listener else {
        return Vec::new();
    };
    if !listener.ip().is_unspecified() {
        return vec![listener];
    }
    let mut addresses: Vec<SocketAddr> = if_addrs::get_if_addrs()
        .map(|interfaces| {
            interfaces
                .into_iter()
                .filter_map(|interface| match interface.ip() {
                    std::net::IpAddr::V4(ip) => Some(SocketAddr::new(ip.into(), listener.port())),
                    std::net::IpAddr::V6(_) => None,
                })
                .collect()
        })
        .unwrap_or_default();
    addresses.push(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), listener.port()));
    addresses.sort();
    addresses.dedup();
    addresses
}

/// Run one bounded scan on `interface`. `own_addresses` (see [`own_addresses`]) are excluded so a
/// relay never lists itself. Blocking; the caller runs it on a blocking thread.
pub fn scan(
    interface: Ipv4Addr,
    discovery_port: u16,
    own_addresses: &[SocketAddr],
) -> Result<HqpDiscoveryObservation, String> {
    if interface.is_unspecified() || interface.is_multicast() {
        return Err("discovery requires an explicit local IPv4 interface".into());
    }
    let started = Instant::now();
    let socket = UdpSocket::bind((interface, 0)).map_err(|e| e.to_string())?;
    let sock = socket2::SockRef::from(&socket);
    sock.set_multicast_if_v4(&interface)
        .map_err(|e| e.to_string())?;
    socket.set_multicast_ttl_v4(1).map_err(|e| e.to_string())?;
    for group in GROUPS {
        socket
            .send_to(QUERY, (group, discovery_port))
            .map_err(|e| e.to_string())?;
    }
    let endpoints = collect(&socket, own_addresses, SCAN_BUDGET).map_err(|e| e.to_string())?;
    Ok(HqpDiscoveryObservation {
        scanned_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        interface: interface.to_string(),
        duration_ms: started.elapsed().as_millis() as u64,
        provenance: "naa-multicast-scan".to_string(),
        endpoints,
    })
}

fn parse(
    packet: &[u8],
    peer: SocketAddr,
    own_addresses: &[SocketAddr],
) -> Option<HqpDiscoveredEndpoint> {
    if packet.len() > 4096
        || peer.port() == 0
        || peer.ip().is_multicast()
        || peer.ip().is_unspecified()
    {
        return None;
    }
    // Exact self exclusion only: the same port on another host is a real endpoint.
    if own_addresses.contains(&peer) {
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
    // Exclude other relays too: never form a chain that can loop back.
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
        || protocol.is_empty()
        || protocol.len() > 16
        || !protocol.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    Some(HqpDiscoveredEndpoint {
        name: name.into(),
        host: peer.ip().to_string(),
        port: peer.port(),
        protocol: protocol.into(),
    })
}

fn collect(
    socket: &UdpSocket,
    own_addresses: &[SocketAddr],
    budget: Duration,
) -> io::Result<Vec<HqpDiscoveredEndpoint>> {
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
                if let Some(endpoint) = parse(&buf[..n], peer, own_addresses) {
                    if found.len() < MAX_RESULTS || found.contains_key(&peer) {
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

// One receiver per discovery bind; registrations own their reply socket and ACL. Keeping the
// registry locked through final shutdown prevents a replacement racing a still-bound receiver.
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;

struct Advertisement {
    socket: Arc<UdpSocket>,
    bytes: Vec<u8>,
    allow: Vec<IpAddr>,
}
struct Hub {
    socket: Arc<UdpSocket>,
    entries: Arc<Mutex<BTreeMap<SocketAddr, Advertisement>>>,
    interfaces: Vec<Ipv4Addr>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}
impl Drop for Hub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}
static HUBS: OnceLock<Mutex<BTreeMap<SocketAddr, Hub>>> = OnceLock::new();
fn hubs() -> &'static Mutex<BTreeMap<SocketAddr, Hub>> {
    HUBS.get_or_init(Mutex::default)
}
fn guard<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub struct Registration {
    receiver: SocketAddr,
    endpoint: SocketAddr,
}
impl Drop for Registration {
    fn drop(&mut self) {
        let mut hubs = guard(hubs());
        let empty = if let Some(hub) = hubs.get(&self.receiver) {
            let mut entries = guard(&hub.entries);
            entries.remove(&self.endpoint);
            entries.is_empty()
        } else {
            false
        };
        if empty {
            hubs.remove(&self.receiver);
        }
    }
}

pub fn register(
    interface: Ipv4Addr,
    discovery_port: u16,
    tcp_port: u16,
    name: &str,
    allow: &[IpAddr],
) -> Result<Registration, String> {
    let bytes = advertisement(name)?;
    let receiver = SocketAddr::from((
        if interface.is_loopback() {
            interface
        } else {
            Ipv4Addr::UNSPECIFIED
        },
        discovery_port,
    ));
    let endpoint = SocketAddr::from((interface, tcp_port));
    let mut hubs = guard(hubs());
    if let std::collections::btree_map::Entry::Vacant(entry) = hubs.entry(receiver) {
        let socket = Arc::new(
            UdpSocket::bind(receiver)
                .map_err(|e| format!("cannot bind NAA discovery receiver {receiver}: {e}"))?,
        );
        socket
            .set_read_timeout(Some(Duration::from_millis(100)))
            .map_err(|e| e.to_string())?;
        let entries = Arc::new(Mutex::new(BTreeMap::<SocketAddr, Advertisement>::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_socket = socket.clone();
        let worker_entries = entries.clone();
        let worker_stop = stop.clone();
        let join = std::thread::Builder::new()
            .name(format!("naa-discovery-{discovery_port}"))
            .spawn(move || {
                let mut buf = [0; 4097];
                while !worker_stop.load(Ordering::Acquire) {
                    match worker_socket.recv_from(&mut buf) {
                        Ok((n, peer)) if valid_request(&buf[..n]) => {
                            for advert in guard(&worker_entries).values() {
                                if advert.allow.is_empty() || advert.allow.contains(&peer.ip()) {
                                    let _ = advert.socket.send_to(&advert.bytes, peer);
                                }
                            }
                        }
                        Ok(_) => {}
                        Err(e)
                            if matches!(
                                e.kind(),
                                io::ErrorKind::WouldBlock
                                    | io::ErrorKind::TimedOut
                                    | io::ErrorKind::Interrupted
                            ) => {}
                        Err(e) => {
                            tracing::warn!(%e, "NAA discovery receiver stopped");
                            break;
                        }
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        entry.insert(Hub {
            socket,
            entries,
            interfaces: Vec::new(),
            stop,
            join: Some(join),
        });
    }
    let result = (|| {
        let hub = hubs
            .get_mut(&receiver)
            .ok_or("discovery receiver unavailable")?;
        if guard(&hub.entries).contains_key(&endpoint) {
            return Err("relay endpoint already advertised".into());
        }
        if !hub.interfaces.contains(&interface) {
            for group in GROUPS {
                hub.socket
                    .join_multicast_v4(&group, &interface)
                    .map_err(|e| format!("cannot join NAA discovery on {interface}: {e}"))?;
            }
            hub.interfaces.push(interface);
        }
        // Legacy listeners can keep using the discovery port. All other advertisements must
        // originate at the TCP port: HQPlayer takes the reply's source port as the endpoint.
        let socket = if tcp_port == discovery_port {
            hub.socket.clone()
        } else {
            Arc::new(
                UdpSocket::bind(endpoint)
                    .map_err(|e| format!("cannot bind NAA advertisement {endpoint}: {e}"))?,
            )
        };
        let allow = if interface.is_loopback() && allow.is_empty() {
            vec![interface.into()]
        } else {
            allow.to_vec()
        };
        guard(&hub.entries).insert(
            endpoint,
            Advertisement {
                socket,
                bytes,
                allow,
            },
        );
        Ok(Registration { receiver, endpoint })
    })();
    if result.is_err()
        && hubs
            .get(&receiver)
            .is_some_and(|hub| guard(&hub.entries).is_empty())
    {
        hubs.remove(&receiver);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(name: &str, os: &str) -> Vec<u8> {
        format!("<?xml version=\"1.0\" encoding=\"utf-8\"?><networkaudio><discover name=\"{name}\" os=\"{os}\" protocol=\"6\" result=\"OK\" trigger=\"1\" version=\"fixture 0.1\">network audio</discover></networkaudio>\0").into_bytes()
    }

    #[test]
    fn rejects_self_other_relays_requests_and_malformed_packets() {
        let peer: SocketAddr = "127.0.0.1:43210".parse().unwrap_or_else(|_| unreachable!());
        let other: SocketAddr = "127.0.0.2:43210".parse().unwrap_or_else(|_| unreachable!());
        assert!(
            parse(&reply("room", "linux"), peer, &[peer]).is_none(),
            "self excluded"
        );
        assert!(
            parse(QUERY, peer, &[other]).is_none(),
            "a query is not an answer"
        );
        assert!(parse(&reply("room", "linux"), peer, &[other]).is_some());
        assert!(
            parse(&reply("room", "HiPhi Router"), peer, &[other]).is_none(),
            "no relay chains"
        );
        assert!(parse(b"<!DOCTYPE x><networkaudio/>", peer, &[other]).is_none());
        assert!(parse(&vec![b'x'; 4097], peer, &[other]).is_none());
        assert!(parse(&reply(&"x".repeat(129), "linux"), peer, &[other]).is_none());
    }

    /// Finding 25: an advertisement (result-bearing) is a reply, never a request, so two
    /// responders on one host cannot answer each other.
    #[test]
    fn advertisements_are_not_valid_requests_but_real_requests_are() {
        let advert = advertisement("Living Room").unwrap_or_default();
        assert!(!valid_request(&advert), "a reply must not trigger a reply");
        assert!(valid_request(
            b"<?xml version=\"1.0\"?><networkaudio><discover version=\"Signalyst HQPlayer Embedded\">network audio</discover></networkaudio>\0"
        ));
        assert!(valid_request(QUERY));
        assert!(!valid_request(
            b"<networkaudio><discover result=\"OK\">network audio</discover></networkaudio>\0"
        ));
        assert!(!valid_request(b"<discover>network audio</discover>"));
    }

    /// A wildcard-bound relay must exclude only its own concrete addresses. Every remote NAA on
    /// the standard port stays discoverable.
    #[test]
    fn wildcard_bind_excludes_only_local_addresses_not_every_remote_endpoint_on_the_port() {
        let wildcard: SocketAddr = "0.0.0.0:43210".parse().unwrap_or_else(|_| unreachable!());
        let own = own_addresses(Some(wildcard));
        assert!(
            own.iter().all(|a| !a.ip().is_unspecified()),
            "expanded to concrete addresses: {own:?}"
        );
        assert!(
            own.contains(&"127.0.0.1:43210".parse().unwrap_or_else(|_| unreachable!())),
            "loopback self is excluded: {own:?}"
        );
        let remote: SocketAddr = "192.0.2.77:43210"
            .parse()
            .unwrap_or_else(|_| unreachable!());
        assert!(
            parse(&reply("Remote NAA", "linux"), remote, &own).is_some(),
            "a remote NAA on 43210 must stay discoverable behind a wildcard bind"
        );
        let local_self: SocketAddr = "127.0.0.1:43210".parse().unwrap_or_else(|_| unreachable!());
        assert!(parse(&reply("me", "linux"), local_self, &own).is_none());
        assert!(own_addresses(None).is_empty());
    }

    #[test]
    fn duplicate_names_stay_distinct_by_peer_and_a_quiet_scan_is_observed_empty() {
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap_or_else(|_| unreachable!());
        let a = UdpSocket::bind("127.0.0.1:0").unwrap_or_else(|_| unreachable!());
        let b = UdpSocket::bind("127.0.0.1:0").unwrap_or_else(|_| unreachable!());
        let target = receiver.local_addr().unwrap_or_else(|_| unreachable!());
        for sender in [&a, &a, &b] {
            let _ = sender.send_to(&reply("same", "linux"), target);
        }
        let result = collect(&receiver, &[], Duration::from_millis(60)).unwrap_or_default();
        assert_eq!(result.len(), 2, "same name from two peers is two endpoints");
        assert_ne!(result[0].port, result[1].port);
        let quiet = collect(&receiver, &[], Duration::from_millis(40)).unwrap_or_default();
        assert!(
            quiet.is_empty(),
            "a completed quiet scan is observed empty, not unknown"
        );
    }
}
