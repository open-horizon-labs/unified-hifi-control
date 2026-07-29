//! Byte-level wire layer of the HQPlayer conformance boundary.
//!
//! This layer decides **how and when** reply bytes reach the client — whether a document is
//! written whole or split across TCP writes. It never inspects the XML; document content comes
//! from [`super::corpus`] and from the responder the test supplies.
//!
//! Separating "what the daemon says" from "how it says it" is the point of issue #322: the
//! existing `MockHqpServer` fuses both into one `match` arm, so no test can vary one while
//! pinning the other, which is why it can only ever succeed.
//!
//! Nothing here is asserted on elapsed wall-clock time. [`WirePolicy::chunk_delay`] exists only
//! to *order* events — to make a partial write observable before the remainder arrives — and
//! tests assert on what the client concludes, never on how long it took.
//!
//! Framing rules modelled here come from the verified protocol reference:
//! <https://github.com/ohshitgorillas/hqptuner/blob/67557939ae04b157b47cb67bd651b72c3140bcdd/docs/protocol.md>
//! (§1 transport and framing, §6 `Status`).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

/// How one reply document is split across TCP writes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Chunking {
    /// One write per document.
    #[default]
    Whole,
    /// Split immediately after the first occurrence of this marker, so the remainder of the
    /// document arrives in a later TCP segment.
    ///
    /// Pointed at a container's first self-closing child, this reproduces the framing trap #322
    /// names as a hard constraint: a reader must not treat the child's `/>` as the end of the
    /// document.
    AfterMarker(String),
}

/// The wire's byte-level policy.
#[derive(Debug, Clone)]
pub struct WirePolicy {
    pub chunking: Chunking,
    /// Ordering delay between chunks of a split reply. Never asserted on.
    pub chunk_delay: Duration,
}

impl Default for WirePolicy {
    fn default() -> Self {
        Self {
            chunking: Chunking::Whole,
            chunk_delay: Duration::from_millis(20),
        }
    }
}

/// Produces the reply document for one request line. `None` means the daemon stayed silent.
pub trait Responder: Send + Sync + 'static {
    fn respond(&self, request: &str) -> Option<String>;
}

impl<F> Responder for F
where
    F: Fn(&str) -> Option<String> + Send + Sync + 'static,
{
    fn respond(&self, request: &str) -> Option<String> {
        self(request)
    }
}

/// A listening fake HQPlayer endpoint.
pub struct WireServer {
    addr: SocketAddr,
    accept_task: JoinHandle<()>,
}

impl WireServer {
    /// Bind an ephemeral loopback port and start serving.
    pub async fn start(responder: Arc<dyn Responder>, policy: WirePolicy) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("local_addr");

        let accept_task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let responder = responder.clone();
                let policy = policy.clone();
                tokio::spawn(async move {
                    serve_connection(stream, responder, policy).await;
                });
            }
        });

        Self { addr, accept_task }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    pub fn stop(self) {
        self.accept_task.abort();
    }
}

fn split(document: &[u8], chunking: &Chunking) -> Vec<Vec<u8>> {
    match chunking {
        Chunking::Whole => vec![document.to_vec()],
        Chunking::AfterMarker(marker) => {
            let hay = String::from_utf8_lossy(document).into_owned();
            match hay.find(marker.as_str()) {
                Some(at) => {
                    let cut = at + marker.len();
                    vec![document[..cut].to_vec(), document[cut..].to_vec()]
                }
                None => vec![document.to_vec()],
            }
        }
    }
}

async fn serve_connection(stream: TcpStream, responder: Arc<dyn Responder>, policy: WirePolicy) {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }

        // The daemon tolerates inter-document whitespace; a bare space is the documented
        // application keep-alive and draws no reply.
        if line.trim().is_empty() {
            continue;
        }

        let Some(reply) = responder.respond(&line) else {
            continue;
        };

        // Documents are newline-terminated on the wire. Internal newlines are legal, and are
        // exactly what makes a line-per-document reader misframe a container response.
        let mut bytes = reply.into_bytes();
        if !bytes.ends_with(b"\n") {
            bytes.push(b'\n');
        }

        let chunks = split(&bytes, &policy.chunking);
        let last = chunks.len().saturating_sub(1);
        for (i, chunk) in chunks.iter().enumerate() {
            if writer.write_all(chunk).await.is_err() || writer.flush().await.is_err() {
                return;
            }
            if i < last && !policy.chunk_delay.is_zero() {
                tokio::time::sleep(policy.chunk_delay).await;
            }
        }
    }
}
