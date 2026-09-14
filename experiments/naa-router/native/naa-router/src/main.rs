mod control;
mod discovery;
mod http;
mod protocol;
mod state;
use clap::Parser;
use state::Router;
use std::{
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, UdpSocket},
    path::PathBuf,
    sync::Arc,
    thread,
    time::Duration,
};

/// Route HQPlayer through one stable NAA identity to an explicitly selected endpoint.
/// This private PoC forwards authentication and audio; it performs no DSP.
#[derive(Parser, Debug)]
#[command(version)]
struct Args {
    /// NAA TCP listener. Use a specific LAN address for HQPlayer on another machine.
    #[arg(long, default_value = "127.0.0.1:43210")]
    naa_bind: SocketAddr,
    /// Local browser/API listener (loopback is recommended).
    #[arg(long, default_value = "127.0.0.1:8787")]
    control_bind: SocketAddr,
    /// Stable adapter and virtual DAC display name.
    #[arg(long, default_value = "HiPhi Router")]
    name: String,
    /// Accept NAA/discovery only from these HQPlayer IPs (repeatable).
    #[arg(long)]
    hqp_allow: Vec<IpAddr>,
    /// Persist routes and selection as JSON. Omit for an in-memory session.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Opt in to NAA discovery on this explicit local IPv4 interface.
    #[arg(long)]
    discovery_interface: Option<Ipv4Addr>,
    /// NAA UDP discovery port; TCP must use the same port when discovery is enabled.
    #[arg(long, default_value_t = 43210)]
    discovery_port: u16,
    /// Optional HQPlayer native control address (host IP:4321) for one-click
    /// route changes: Stop, re-route, wait for the new NAA session, Play. No
    /// discovery or guessing; omit to keep the pure proxy behavior.
    #[arg(long)]
    hqp_control: Option<SocketAddr>,
}
fn main() {
    if let Err(e) = run() {
        eprintln!("naa-router: {e}");
        std::process::exit(1);
    }
}
fn run() -> io::Result<()> {
    let args = Args::parse();
    naa_native::discovery::response(&args.name)?;
    if let Some(ip) = args.discovery_interface {
        if ip.is_unspecified()
            || ip.is_multicast()
            || args.naa_bind.port() != args.discovery_port
            || (!args.naa_bind.ip().is_unspecified()
                && args.naa_bind.ip() != std::net::IpAddr::V4(ip))
        {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "discovery requires an explicit interface matching --naa-bind and the same TCP/UDP port"));
        }
    }
    if !args.naa_bind.ip().is_loopback() && args.hqp_allow.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "LAN exposure requires --hqp-allow with the explicit HQPlayer IP",
        ));
    }
    if !args.control_bind.ip().is_loopback() {
        eprintln!(
            "naa-router: WARNING control API on {} is unauthenticated; anyone who can reach it can change routes. Loopback is recommended.",
            args.control_bind
        );
    }
    let naa = TcpListener::bind(args.naa_bind)?;
    let http = TcpListener::bind(args.control_bind)?;
    let router = Arc::new(Router::new(
        args.name,
        naa.local_addr()?,
        http.local_addr()?,
        args.config,
        args.hqp_control,
    )?);
    if let Some(ip) = args.discovery_interface {
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, args.discovery_port))?;
        for group in naa_native::discovery::GROUPS {
            socket.join_multicast_v4(&group, &ip)?;
        }
        socket.set_read_timeout(Some(Duration::from_secs(1)))?;
        let r = router.clone();
        let allowed = args.hqp_allow.clone();
        thread::spawn(move || {
            let mut buf = [0; 4097];
            let mut reply = naa_native::discovery::response(&r.name).unwrap();
            // Reuse observed discovery shape; do not claim this host is Android.
            reply = String::from_utf8(reply)
                .unwrap()
                .replace("os=\"android\"", "os=\"HiPhi Router\"")
                .replace("HiPhi native adapter 0.1", "HiPhi Router PoC 0.1")
                .into_bytes();
            loop {
                match socket.recv_from(&mut buf) {
                    Ok((n, peer))
                        if (allowed.is_empty() || allowed.contains(&peer.ip()))
                            && naa_native::discovery::valid_request(&buf[..n]) =>
                    {
                        let _ = socket.send_to(&reply, peer);
                    }
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                        ) => {}
                    Err(e) => {
                        eprintln!("discovery: {e}");
                        break;
                    }
                    _ => {}
                }
            }
        });
    }
    let r = router.clone();
    let discovery = Arc::new(discovery::Discovery::new(
        args.discovery_interface,
        router.naa_addr,
    ));
    thread::spawn(move || http::serve(http, r, discovery));
    println!(
        "HiPhi Router NAA={} control=http://{}",
        router.naa_addr, router.control_addr
    );
    println!("Choose a downstream endpoint in the browser, then select {} in HQPlayer. Route changes disconnect the current NAA session.", router.name);
    if let Some(addr) = router.hqp_control {
        println!("HQPlayer control {addr}: a playing source is stopped, re-routed and resumed with a single Play; stopped or paused sources are only re-routed.");
    }
    for client in naa.incoming() {
        match client {
            Ok(client) => {
                if !args.hqp_allow.is_empty()
                    && !client
                        .peer_addr()
                        .is_ok_and(|a| args.hqp_allow.contains(&a.ip()))
                {
                    let _ = client.shutdown(std::net::Shutdown::Both);
                    continue;
                }
                let r = router.clone();
                // Reserve the exclusive session before spawning: connection floods do not create unbounded workers.
                match r.reserve(&client) {
                    Ok((id, route)) => {
                        thread::spawn(move || protocol::serve(client, r, id, route));
                    }
                    Err(_) => {
                        let _ = client.shutdown(std::net::Shutdown::Both);
                    }
                }
            }
            Err(e) => eprintln!("NAA accept: {e}"),
        }
    }
    Ok(())
}
