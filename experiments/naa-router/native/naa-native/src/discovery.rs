//! Discovery shape observed from the reference endpoint on 2026-09-13.
use std::{
    io,
    net::{Ipv4Addr, UdpSocket},
};
pub const GROUPS: [Ipv4Addr; 2] = [
    Ipv4Addr::new(224, 0, 0, 199),
    Ipv4Addr::new(239, 192, 0, 199),
];
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
}
pub fn response(name: &str) -> io::Result<Vec<u8>> {
    if name.is_empty() || name.len() > 256 || name.chars().any(char::is_control) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "DISCOVERY_NAME",
        ));
    }
    let name = name
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    Ok(format!("<?xml version=\"1.0\" encoding=\"utf-8\"?><networkaudio><discover name=\"{name}\" os=\"android\" protocol=\"6\" result=\"OK\" trigger=\"1\" version=\"HiPhi native adapter 0.1\">network audio</discover></networkaudio>\0").into_bytes())
}
pub fn serve(socket: UdpSocket, name: String) -> io::Result<()> {
    let reply = response(&name)?;
    let mut buffer = [0; 4097];
    loop {
        let (n, peer) = socket.recv_from(&mut buffer)?;
        if valid_request(&buffer[..n]) {
            socket.send_to(&reply, peer)?;
        }
    }
}
pub fn bind(port: u16) -> io::Result<UdpSocket> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port))?;
    for group in GROUPS {
        socket.join_multicast_v4(&group, &Ipv4Addr::UNSPECIFIED)?;
    }
    Ok(socket)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_discovery_shape() {
        assert!(valid_request(b"<?xml version=\"1.0\"?><networkaudio><discover version=\"Signalyst HQPlayer Desktop 5\">network audio</discover></networkaudio>\0"));
        assert!(!valid_request(b"<discover/>"));
        assert!(!valid_request(b"<!-- <discover --><networkaudio/>"));
        let r = response("A&B").unwrap();
        assert!(r.ends_with(&[0]));
        let d =
            roxmltree::Document::parse(std::str::from_utf8(&r[..r.len() - 1]).unwrap()).unwrap();
        assert_eq!(
            d.root_element()
                .first_element_child()
                .unwrap()
                .attribute("name"),
            Some("A&B")
        );
    }
}
