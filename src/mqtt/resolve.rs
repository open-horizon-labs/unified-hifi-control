//! Broker host resolution for the MQTT publisher.
//!
//! UHC ships as a static musl binary, whose resolver reads `/etc/hosts` and
//! `resolv.conf` and nothing else. On a NAS whose `nsswitch` has no mDNS
//! module, `homeassistant.local` - the address Home Assistant itself tells
//! people to use - therefore never resolves, even though Home Assistant
//! answers mDNS on the same LAN. So a `.local` broker host is resolved over
//! mDNS here, inside UHC, before `rumqttc` dials it.
//!
//! Only the address handed to `rumqttc` changes. The host the user typed is
//! what is stored and reported, and resolution runs again before every
//! connection attempt, so a broker that moves to a new DHCP lease is found
//! at its new address on the next reconnect.

use std::net::IpAddr;
use std::time::Duration;

use async_trait::async_trait;

/// How long one mDNS lookup may take before falling back to the system
/// resolver.
pub const MDNS_RESOLVE_TIMEOUT: Duration = Duration::from_secs(3);

/// Something that can answer "which address does this `.local` name have".
/// A trait so the decision below can be tested without a live mDNS
/// responder on the network.
#[async_trait]
pub trait MdnsResolver: Send + Sync {
    /// Resolve `host` (as typed, e.g. `homeassistant.local`) over mDNS.
    /// `None` when nothing answered within `timeout`.
    async fn resolve(&self, host: &str, timeout: Duration) -> Option<IpAddr>;
}

/// Where the address `rumqttc` dials came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// A `.local` host answered over mDNS.
    Mdns(IpAddr),
    /// A `.local` host nothing answered for over mDNS; the system resolver
    /// gets the name as typed (it may still know it, e.g. from /etc/hosts).
    MdnsMissed,
    /// Not a `.local` host: the system resolver handles it as before.
    System,
}

/// The address to give `rumqttc` for one connection attempt, and how it was
/// obtained.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectTarget {
    pub host: String,
    pub resolution: Resolution,
}

/// Whether `host` is a multicast-DNS name (`*.local`, any case, with or
/// without the trailing root dot).
pub fn is_mdns_host(host: &str) -> bool {
    let name = host.strip_suffix('.').unwrap_or(host);
    let Some(split) = name.len().checked_sub(".local".len()) else {
        return false;
    };
    split > 0 && name.is_char_boundary(split) && name[split..].eq_ignore_ascii_case(".local")
}

/// Decide what `rumqttc` should dial for `host`: an mDNS-resolved address
/// for a `.local` name, otherwise the name itself for the system resolver.
pub async fn connect_target(
    host: &str,
    mdns: &dyn MdnsResolver,
    timeout: Duration,
) -> ConnectTarget {
    if !is_mdns_host(host) {
        return ConnectTarget {
            host: host.to_string(),
            resolution: Resolution::System,
        };
    }
    match mdns.resolve(host, timeout).await {
        Some(address) => ConnectTarget {
            // `rumqttc` dials `format!("{host}:{port}")`, so an IPv6
            // literal needs its brackets to survive that.
            host: match address {
                IpAddr::V4(v4) => v4.to_string(),
                IpAddr::V6(v6) => format!("[{v6}]"),
            },
            resolution: Resolution::Mdns(address),
        },
        None => ConnectTarget {
            host: host.to_string(),
            resolution: Resolution::MdnsMissed,
        },
    }
}

/// The real resolver, backed by the same `mdns-sd` crate UHC advertises
/// itself with. The daemon (a thread plus multicast sockets) is only
/// started the first time a `.local` broker actually needs resolving, and
/// is kept for the publisher task's lifetime so reconnects reuse it.
#[derive(Default)]
pub struct MdnsSdResolver {
    daemon: std::sync::Mutex<Option<mdns_sd::ServiceDaemon>>,
}

impl MdnsSdResolver {
    pub fn new() -> Self {
        Self::default()
    }

    fn daemon(&self) -> Option<mdns_sd::ServiceDaemon> {
        let mut guard = self.daemon.lock().ok()?;
        if guard.is_none() {
            match mdns_sd::ServiceDaemon::new() {
                Ok(daemon) => *guard = Some(daemon),
                Err(error) => {
                    tracing::warn!("MQTT: could not start an mDNS resolver: {error}");
                    return None;
                }
            }
        }
        guard.clone()
    }

    /// Stop the daemon, if one was ever started.
    pub fn shutdown(&self) {
        let daemon = self.daemon.lock().ok().and_then(|mut guard| guard.take());
        if let Some(daemon) = daemon {
            let _ = daemon.shutdown();
        }
    }
}

impl Drop for MdnsSdResolver {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[async_trait]
impl MdnsResolver for MdnsSdResolver {
    async fn resolve(&self, host: &str, timeout: Duration) -> Option<IpAddr> {
        use mdns_sd::HostnameResolutionEvent;

        let daemon = self.daemon()?;
        // mdns-sd wants the fully-qualified form, `name.local.`.
        let name = host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase();
        let fqdn = format!("{name}.");
        let receiver = match daemon.resolve_hostname(&fqdn, Some(timeout.as_millis() as u64)) {
            Ok(receiver) => receiver,
            Err(error) => {
                tracing::debug!(host, "MQTT: mDNS lookup could not start: {error}");
                return None;
            }
        };
        let found = tokio::time::timeout(timeout + Duration::from_millis(250), async {
            while let Ok(event) = receiver.recv_async().await {
                match event {
                    HostnameResolutionEvent::AddressesFound(_, addresses) => {
                        let addresses: Vec<IpAddr> =
                            addresses.iter().map(|ip| ip.to_ip_addr()).collect();
                        if let Some(address) = preferred_address(&addresses) {
                            return Some(address);
                        }
                    }
                    HostnameResolutionEvent::SearchTimeout(_)
                    | HostnameResolutionEvent::SearchStopped(_) => return None,
                    _ => {}
                }
            }
            None
        })
        .await
        .ok()
        .flatten();
        let _ = daemon.stop_resolve_hostname(&fqdn);
        found
    }
}

/// IPv4 first: it is what a home broker is reachable on in practice, and an
/// IPv6 link-local address needs a scope id the dialler has no way to take.
fn preferred_address(addresses: &[IpAddr]) -> Option<IpAddr> {
    addresses
        .iter()
        .find(|address| address.is_ipv4())
        .or_else(|| {
            addresses.iter().find(|address| match address {
                IpAddr::V6(v6) => !v6.is_unicast_link_local(),
                IpAddr::V4(_) => false,
            })
        })
        .copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use std::sync::Mutex;

    /// Records what it was asked and answers with a fixed address.
    struct FakeMdns {
        answer: Option<IpAddr>,
        asked: Mutex<Vec<String>>,
    }

    impl FakeMdns {
        fn answering(answer: Option<IpAddr>) -> Self {
            Self {
                answer,
                asked: Mutex::new(Vec::new()),
            }
        }

        fn asked(&self) -> Vec<String> {
            self.asked.lock().map(|g| g.clone()).unwrap_or_default()
        }
    }

    #[async_trait]
    impl MdnsResolver for FakeMdns {
        async fn resolve(&self, host: &str, _timeout: Duration) -> Option<IpAddr> {
            if let Ok(mut asked) = self.asked.lock() {
                asked.push(host.to_string());
            }
            self.answer
        }
    }

    const HA: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 209));

    #[test]
    fn recognises_local_names_in_any_case_and_with_a_root_dot() {
        assert!(is_mdns_host("homeassistant.local"));
        assert!(is_mdns_host("HomeAssistant.LOCAL"));
        assert!(is_mdns_host("homeassistant.local."));
        assert!(!is_mdns_host("local"));
        assert!(!is_mdns_host(".local"));
        assert!(!is_mdns_host("core-mosquitto"));
        assert!(!is_mdns_host("broker.lan"));
        assert!(!is_mdns_host("notlocal"));
        assert!(!is_mdns_host("192.168.1.209"));
    }

    #[tokio::test]
    async fn a_local_host_is_dialled_at_its_mdns_address() {
        let mdns = FakeMdns::answering(Some(HA));
        let target = connect_target("homeassistant.local", &mdns, MDNS_RESOLVE_TIMEOUT).await;
        assert_eq!(
            target,
            ConnectTarget {
                host: "192.168.1.209".to_string(),
                resolution: Resolution::Mdns(HA),
            }
        );
        assert_eq!(mdns.asked(), vec!["homeassistant.local".to_string()]);
    }

    #[tokio::test]
    async fn a_local_host_mdns_cannot_find_falls_back_to_the_system_resolver() {
        let mdns = FakeMdns::answering(None);
        let target = connect_target("HomeAssistant.Local", &mdns, MDNS_RESOLVE_TIMEOUT).await;
        assert_eq!(
            target,
            ConnectTarget {
                host: "HomeAssistant.Local".to_string(),
                resolution: Resolution::MdnsMissed,
            }
        );
        assert_eq!(mdns.asked().len(), 1);
    }

    #[tokio::test]
    async fn other_hosts_never_touch_mdns() {
        let mdns = FakeMdns::answering(Some(HA));
        for host in [
            "core-mosquitto",
            "broker.lan",
            "192.168.1.5",
            "mqtt.example.com",
        ] {
            let target = connect_target(host, &mdns, MDNS_RESOLVE_TIMEOUT).await;
            assert_eq!(
                target,
                ConnectTarget {
                    host: host.to_string(),
                    resolution: Resolution::System,
                }
            );
        }
        assert!(
            mdns.asked().is_empty(),
            "mDNS must not be asked about {:?}",
            mdns.asked()
        );
    }

    #[test]
    fn prefers_ipv4_and_never_picks_a_link_local_ipv6_address() {
        let link_local: IpAddr = "fe80::1".parse().expect("v6");
        let global: IpAddr = "fd00::209".parse().expect("v6");
        assert_eq!(preferred_address(&[link_local, global, HA]), Some(HA));
        assert_eq!(preferred_address(&[link_local, global]), Some(global));
        assert_eq!(preferred_address(&[link_local]), None);
    }

    #[tokio::test]
    async fn an_ipv6_mdns_answer_is_bracketed_for_the_dialler() {
        let v6: IpAddr = "fd00::209".parse().expect("valid v6");
        let mdns = FakeMdns::answering(Some(v6));
        let target = connect_target("homeassistant.local", &mdns, MDNS_RESOLVE_TIMEOUT).await;
        assert_eq!(target.host, "[fd00::209]");
        assert_eq!(target.resolution, Resolution::Mdns(v6));
    }
}
