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
use std::sync::atomic::{AtomicU32, Ordering};
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
    /// Split `extra` **bytes** past the first occurrence of `marker`.
    ///
    /// [`Self::AfterMarker`] cuts at a `&str` boundary and so can never land inside a multi-byte code
    /// point. This can, which is the whole reason it exists: the client classifies only the longest
    /// valid UTF-8 prefix of its buffer precisely so a character straddling a read is excluded from
    /// that attempt rather than mangled, and a char-aligned cut cannot exercise that path at all.
    ///
    /// The offset is in bytes on purpose. A test computes it so the cut falls between the bytes of a
    /// known character, and asserts that premise itself rather than trusting the arithmetic.
    BytesAfterMarker { marker: String, extra: usize },
}

/// What the wire does to the connection instead of, or after, replying (AC2 reconnect boundaries).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Disruption {
    #[default]
    None,
    /// Close the connection without replying to the next request, once. The client must reconnect
    /// and retry to make progress.
    DropNextReplyOnce,
    /// Apply the next request to the model, then close the connection without replying, once.
    ///
    /// The mirror image of [`Self::DropNextReplyOnce`] and the one that matters for at-most-once
    /// semantics: from the client's side the two are **indistinguishable** — a request was written and
    /// no reply came back. The difference is invisible to it and decisive for the user, because a
    /// blind retry of `Next` or `VolumeUp` applies the side effect a second time. (`VolumeMute` is
    /// *not* such a command: live validation showed it is an absolute mute-to-floor and idempotent, so
    /// it is retried like any absolute setter.)
    /// `DropNextReplyOnce` alone can never expose that: it returns before the model is touched, so
    /// every retry is harmless there by construction.
    ApplyThenDropReplyOnce,
}

/// The wire's byte-level policy.
#[derive(Debug, Clone)]
pub struct WirePolicy {
    pub chunking: Chunking,
    /// Ordering delay between chunks of a split reply. Never asserted on.
    pub chunk_delay: Duration,
    pub disruption: Disruption,
    /// Requests for this element draw no reply at all, so the client must hit its response timeout.
    pub silent_for_element: Option<String>,
    /// Apply this element to the model, then close the connection without replying — **once**.
    ///
    /// The element-scoped mirror of [`Disruption::ApplyThenDropReplyOnce`], which fires on whichever
    /// request comes next. That is enough when a client operation is one command, and not enough when
    /// it is several: since #347 a setter reads an enumeration and `State` before it writes, so the
    /// unscoped form vanishes during the *read* and the write never happens. Naming the element puts
    /// the drop where the claim is — after the daemon applied the write, before the reply.
    pub apply_then_drop_reply_for_element: Option<String>,
    /// Append these raw bytes instead of a reply for this element, to model a malformed document.
    pub malformed_for_element: Option<(String, String)>,
    /// `(element, document, interval)` — on a request for that element, stream `document`
    /// repeatedly at `interval` and **never** send the real reply.
    ///
    /// Models the daemon pushing `Status` frames while a command goes unanswered. The point is that
    /// a per-read timeout resets on every one of these, so without an overall per-command deadline
    /// the client stays alive for as long as the stream continues.
    pub unsolicited_stream_for_element: Option<(String, String, Duration)>,
    /// `(element, at_least_bytes)` — answer that element with a root element that **never closes**,
    /// padded past `at_least_bytes`, then fall silent while leaving the connection open.
    ///
    /// Models a daemon whose reply is unbounded — a corrupted or runaway container. The client is
    /// accumulating a `String` per command, so without an explicit byte ceiling the only thing
    /// bounding that buffer is the response deadline, which is a time bound and not a memory bound.
    /// A cap turns unbounded growth into a reported error.
    ///
    /// Fires **once** per server so a test can prove the *next* command still works, which is the
    /// recovery half of the requirement.
    pub oversized_for_element: Option<(String, usize)>,
    /// `(element, at_least_bytes)` — like [`Self::oversized_for_element`], but **without a single
    /// newline anywhere**.
    ///
    /// This is the shape a line-oriented reader cannot defend against. `read_line` grows its target
    /// `String` until it finds a `\n` or the peer closes, so a check performed *after* the read has
    /// already allocated whatever the daemon chose to send. A cap that runs post-read is a bound on
    /// what is *retained*, not on what is *allocated*, and those are different guarantees.
    ///
    /// Fires once per server, so a test can prove the next command recovers.
    pub oversized_newline_free_for_element: Option<(String, usize)>,
    /// `(element, leading_document)` — **prepend** an unsolicited document *before* the real reply, in
    /// the same TCP write, so one read delivers the follower first and the wanted reply behind it.
    ///
    /// The mirror image of [`Self::coalesce_extra_for_element`], and the harder case. A reader that
    /// drains the unsolicited document and then goes straight back to the socket blocks on a read it
    /// does not need, because the reply it wants is already in the buffer. The daemon pushes `Status`
    /// frames of its own accord, so a push landing just ahead of a reply is ordinary traffic.
    pub coalesce_leading_for_element: Option<(String, String)>,
    /// `(element, extra_document)` — after replying to that element, append a second, unsolicited
    /// document to the **same TCP write**, so both land in the client's receive buffer together.
    ///
    /// This is how coalescing actually reaches a strictly serial client: the daemon emits a
    /// document nobody asked for (its documented `Status` push frames) in the same segment as the
    /// solicited reply. The reference client copes because it splits its receive buffer and feeds
    /// each document to a streaming parser rather than assuming one read is one reply.
    pub coalesce_extra_for_element: Option<(String, String)>,
}

impl Default for WirePolicy {
    fn default() -> Self {
        Self {
            chunking: Chunking::Whole,
            chunk_delay: Duration::from_millis(20),
            disruption: Disruption::None,
            silent_for_element: None,
            apply_then_drop_reply_for_element: None,
            malformed_for_element: None,
            oversized_for_element: None,
            oversized_newline_free_for_element: None,
            coalesce_leading_for_element: None,
            coalesce_extra_for_element: None,
            unsolicited_stream_for_element: None,
        }
    }
}

/// Counters a test asserts on **instead of** measuring elapsed time.
#[derive(Debug, Default)]
pub struct WireStats {
    connections: AtomicU32,
    requests: AtomicU32,
    replies: AtomicU32,
    /// Request element names in arrival order, including requests the wire swallows before the
    /// responder sees them. Lets a test count retries of one command.
    elements: std::sync::Mutex<Vec<String>>,
}

impl WireStats {
    /// How many TCP connections the client has opened. A reconnect shows up here.
    pub fn connections(&self) -> u32 {
        self.connections.load(Ordering::SeqCst)
    }
    pub fn requests(&self) -> u32 {
        self.requests.load(Ordering::SeqCst)
    }
    pub fn replies(&self) -> u32 {
        self.replies.load(Ordering::SeqCst)
    }
    /// How many times the client sent one element. This is the retry-count assertion.
    pub fn element_count(&self, element: &str) -> u32 {
        self.elements
            .lock()
            .expect("wire stats lock")
            .iter()
            .filter(|e| *e == element)
            .count() as u32
    }

    fn record(&self, line: &str) {
        if let Some(name) = super::corpus::element_name(line) {
            self.elements.lock().expect("wire stats lock").push(name);
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
    stats: Arc<WireStats>,
    /// One-shot disruption budget shared across connections, so `DropNextReplyOnce` fires once for
    /// the whole server rather than once per connection.
    disruptions_left: Arc<AtomicU32>,
    /// One-shot budget for `oversized_for_element`, shared across connections for the same reason:
    /// a test proves the *next* command recovers, which needs the fault to stop after firing.
    oversized_left: Arc<AtomicU32>,
    /// One-shot budget for `apply_then_drop_reply_for_element`. Element-scoped, so like the oversized
    /// fault it is armed by being declared and cannot disturb connection setup.
    element_drops_left: Arc<AtomicU32>,
    accept_task: JoinHandle<()>,
}

impl WireServer {
    /// Bind an ephemeral loopback port and start serving.
    pub async fn start(responder: Arc<dyn Responder>, policy: WirePolicy) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("local_addr");
        let stats = Arc::new(WireStats::default());
        // Disruptions start unarmed so connection setup is never the thing under test; a test
        // calls `arm_drop()` once it is connected.
        let disruptions_left = Arc::new(AtomicU32::new(0));
        // The oversized fault, in contrast, is armed by declaring it in the policy: it answers a
        // named element rather than "whatever comes next", so it cannot disturb connection setup.
        let oversized_left = Arc::new(AtomicU32::new(u32::from(
            policy.oversized_for_element.is_some()
                || policy.oversized_newline_free_for_element.is_some(),
        )));

        let element_drops_left = Arc::new(AtomicU32::new(u32::from(
            policy.apply_then_drop_reply_for_element.is_some(),
        )));

        let accept_stats = stats.clone();
        let accept_budget = disruptions_left.clone();
        let accept_oversized = oversized_left.clone();
        let accept_element_drops = element_drops_left.clone();
        let accept_task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                accept_stats.connections.fetch_add(1, Ordering::SeqCst);
                let responder = responder.clone();
                let policy = policy.clone();
                let stats = accept_stats.clone();
                let budget = accept_budget.clone();
                let oversized = accept_oversized.clone();
                let element_drops = accept_element_drops.clone();
                tokio::spawn(async move {
                    serve_connection(
                        stream,
                        responder,
                        policy,
                        stats,
                        budget,
                        oversized,
                        element_drops,
                    )
                    .await;
                });
            }
        });

        Self {
            addr,
            stats,
            disruptions_left,
            oversized_left,
            element_drops_left,
            accept_task,
        }
    }

    /// Whether the one-shot element-scoped apply-then-drop has fired.
    pub fn element_drop_fired(&self) -> bool {
        self.element_drops_left.load(Ordering::SeqCst) == 0
    }

    /// Whether the one-shot oversized reply has been served.
    pub fn oversized_fired(&self) -> bool {
        self.oversized_left.load(Ordering::SeqCst) == 0
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// Observable counters. Assert on these rather than on elapsed time.
    pub fn stats(&self) -> Arc<WireStats> {
        self.stats.clone()
    }

    /// Arm the policy's one-shot disruption. Call after connecting, so setup is never disrupted.
    pub fn arm_disruption(&self) {
        self.disruptions_left.store(1, Ordering::SeqCst);
    }

    /// Whether an armed one-shot disruption has fired.
    pub fn disruption_fired(&self) -> bool {
        self.disruptions_left.load(Ordering::SeqCst) == 0
    }

    pub fn stop(self) {
        self.accept_task.abort();
    }
}

/// Byte offset of the first occurrence of `needle` in `hay`, or `None`.
///
/// One search shared by both marker arms. `AfterMarker` used to search a `String::from_utf8_lossy`
/// copy and then slice the *raw* bytes with the offset it found — but every invalid byte becomes a
/// 3-byte `U+FFFD` in that copy, so from the first one onward the offset no longer indexes the
/// original and the cut lands somewhere other than the marker, which is the one thing this type exists
/// to control. `BytesAfterMarker` already searched bytes for exactly that reason; the two arms
/// disagreed about the same question.
///
/// The empty needle is answered explicitly: `slice::windows(0)` panics, while `str::find("")` returns
/// `Some(0)`. Matching `str::find` keeps the previous behaviour instead of turning a degenerate
/// chunking config into a crash inside a test server.
fn find_bytes(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

fn split(document: &[u8], chunking: &Chunking) -> Vec<Vec<u8>> {
    match chunking {
        Chunking::Whole => vec![document.to_vec()],
        Chunking::AfterMarker(marker) => match find_bytes(document, marker.as_bytes()) {
            Some(at) => {
                let cut = at + marker.len();
                vec![document[..cut].to_vec(), document[cut..].to_vec()]
            }
            None => vec![document.to_vec()],
        },
        Chunking::BytesAfterMarker { marker, extra } => {
            match find_bytes(document, marker.as_bytes()) {
                Some(at) => {
                    let cut = (at + marker.len() + extra).min(document.len());
                    vec![document[..cut].to_vec(), document[cut..].to_vec()]
                }
                None => vec![document.to_vec()],
            }
        }
    }
}

fn mentions(line: &str, element: &str) -> bool {
    // Exact match on the request's root element, not a substring. `line.contains("<Volume")` also
    // fired for `<VolumeUp>`, `<VolumeMute>` and `<VolumeRange>`, so a disruption armed for `Volume`
    // silently swallowed those unrelated commands - a false absence manufactured inside the boundary
    // built to catch them. `element_name` parses the root element (skipping any XML declaration), so
    // only the intended command is affected.
    super::corpus::element_name(line).as_deref() == Some(element)
}

async fn serve_connection(
    stream: TcpStream,
    responder: Arc<dyn Responder>,
    policy: WirePolicy,
    stats: Arc<WireStats>,
    disruptions_left: Arc<AtomicU32>,
    oversized_left: Arc<AtomicU32>,
    element_drops_left: Arc<AtomicU32>,
) {
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
        stats.requests.fetch_add(1, Ordering::SeqCst);
        stats.record(&line);

        if policy.disruption == Disruption::DropNextReplyOnce
            && disruptions_left
                .compare_exchange(1, 0, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        {
            // Vanish mid-command. The client must notice and reconnect.
            return;
        }

        if let Some(ref element) = policy.silent_for_element {
            if mentions(&line, element) {
                continue;
            }
        }

        if let Some((ref element, ref doc, interval)) = policy.unsolicited_stream_for_element {
            if mentions(&line, element) {
                // Bounded so a stuck test cannot leak an unbounded task; far more than any deadline
                // should allow the client to consume.
                for _ in 0..400 {
                    let mut bytes = doc.clone().into_bytes();
                    if !bytes.ends_with(b"\n") {
                        bytes.push(b'\n');
                    }
                    if writer.write_all(&bytes).await.is_err() || writer.flush().await.is_err() {
                        return;
                    }
                    stats.replies.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(interval).await;
                }
                return;
            }
        }

        if let Some((ref element, at_least)) = policy.oversized_newline_free_for_element {
            if mentions(&line, element)
                && oversized_left
                    .compare_exchange(1, 0, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
            {
                // One unbroken run of bytes with no newline and no document end. A reader that waits
                // for a newline before checking any ceiling has already allocated all of it.
                let filler = "x".repeat(64 * 1024);
                let mut written = 0usize;
                if writer
                    .write_all(b"<?xml version=\"1.0\"?><GetFilters junk=\"")
                    .await
                    .is_err()
                {
                    return;
                }
                while written < at_least {
                    if writer.write_all(filler.as_bytes()).await.is_err() {
                        return;
                    }
                    written += filler.len();
                }
                if writer.flush().await.is_err() {
                    return;
                }
                stats.replies.fetch_add(1, Ordering::SeqCst);
                // No newline, no closing quote, no closing tag, connection left open.
                continue;
            }
        }

        if let Some((ref element, at_least)) = policy.oversized_for_element {
            if mentions(&line, element)
                && oversized_left
                    .compare_exchange(1, 0, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
            {
                // A root element that never closes, padded past the requested size. Newlines keep
                // the client's line-oriented reads progressing, so it accumulates rather than
                // blocking on one enormous read — which is the shape that makes an unbounded buffer
                // a memory hazard rather than merely a slow reply.
                //
                // Frames are deliberately large. The client re-classifies its **whole** accumulated
                // buffer after every line, so accumulation costs O(bytes × lines): with 1 KiB frames
                // the client cannot reach a multi-megabyte buffer inside any sane deadline, and the
                // test would then pass or fail on parser throughput instead of on the byte ceiling.
                // Large frames keep the line count low so the ceiling is what the test measures.
                let filler = format!("<Junk pad=\"{}\"/>\n", "x".repeat(256 * 1024));
                let mut written = 0usize;
                if writer
                    .write_all(b"<?xml version=\"1.0\"?><GetFilters>\n")
                    .await
                    .is_err()
                {
                    return;
                }
                while written < at_least {
                    if writer.write_all(filler.as_bytes()).await.is_err() {
                        return;
                    }
                    written += filler.len();
                }
                if writer.flush().await.is_err() {
                    return;
                }
                stats.replies.fetch_add(1, Ordering::SeqCst);
                // Deliberately never send `</GetFilters>` and leave the connection open: only a
                // byte ceiling can end this, and the client must then recover for the next command.
                continue;
            }
        }

        if let Some((ref element, ref garbage)) = policy.malformed_for_element {
            if mentions(&line, element) {
                let mut bytes = garbage.clone().into_bytes();
                if !bytes.ends_with(b"\n") {
                    bytes.push(b'\n');
                }
                if writer.write_all(&bytes).await.is_err() || writer.flush().await.is_err() {
                    return;
                }
                stats.replies.fetch_add(1, Ordering::SeqCst);
                continue;
            }
        }

        let Some(reply) = responder.respond(&line) else {
            continue;
        };

        // The command has now been applied. Vanishing here is the ambiguous case the client cannot
        // resolve: it wrote a request and got nothing back, exactly as in `DropNextReplyOnce`, but the
        // side effect has already happened.
        if let Some(ref element) = policy.apply_then_drop_reply_for_element {
            if mentions(&line, element)
                && element_drops_left
                    .compare_exchange(1, 0, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
            {
                return;
            }
        }

        if policy.disruption == Disruption::ApplyThenDropReplyOnce
            && disruptions_left
                .compare_exchange(1, 0, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        {
            return;
        }

        // Documents are newline-terminated on the wire. Internal newlines are legal, and are
        // exactly what makes a line-per-document reader misframe a container response.
        let mut bytes = reply.into_bytes();
        if !bytes.ends_with(b"\n") {
            bytes.push(b'\n');
        }

        if let Some((ref element, ref leading)) = policy.coalesce_leading_for_element {
            if mentions(&line, element) {
                let mut both = leading.clone().into_bytes();
                if !both.ends_with(b"\n") {
                    both.push(b'\n');
                }
                both.extend_from_slice(&bytes);
                bytes = both;
            }
        }

        if let Some((ref element, ref extra)) = policy.coalesce_extra_for_element {
            if mentions(&line, element) {
                bytes.extend_from_slice(extra.as_bytes());
                if !bytes.ends_with(b"\n") {
                    bytes.push(b'\n');
                }
            }
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
        stats.replies.fetch_add(1, Ordering::SeqCst);
    }
}

/// Harness tests for the splitter itself (CodeRabbit review 4816484338).
///
/// `split` is what decides where a TCP segment boundary lands, so every framing test in the suite
/// rests on it computing the offset it claims to. Tested directly because no framing test can tell a
/// correct cut from a cut that happened to land somewhere harmless.
///
/// **Label: model-fidelity.** Harness behaviour, red against the unmodified harness.
#[cfg(test)]
mod split_tests {
    use super::*;

    /// The defect: `AfterMarker` searched a **lossy** UTF-8 copy and then sliced the raw bytes with
    /// the offset it found. Every invalid byte becomes a 3-byte `U+FFFD` in that copy, so from the
    /// first one onward the offset no longer indexes the original — the cut lands somewhere else than
    /// the marker, which is the one thing this type exists to control.
    ///
    /// `BytesAfterMarker` already searched bytes for exactly this reason; the two arms disagreed.
    #[test]
    fn after_marker_cuts_at_the_marker_even_after_an_invalid_byte() {
        // `0xFF` is not valid UTF-8. It stands in for the arbitrary bytes a daemon can put on the
        // wire — the suite already models a non-UTF-8 buffer elsewhere, so this is a reachable shape
        // for the splitter even though today's callers all pass documents built from `String`.
        let mut document = b"<Status>".to_vec();
        document.push(0xFF);
        document.extend_from_slice(b"<metadata/></Status>");

        let parts = split(&document, &Chunking::AfterMarker("<metadata/>".to_string()));

        assert_eq!(parts.len(), 2, "the marker is present, so it must split");
        let cut = parts[0].len();
        assert_eq!(
            &document[..cut],
            parts[0].as_slice(),
            "the first part must be a prefix of the original bytes"
        );
        assert!(
            parts[0].ends_with(b"<metadata/>"),
            "the cut must fall immediately after the marker; got {:?}",
            String::from_utf8_lossy(&parts[0])
        );
        assert_eq!(parts[1], b"</Status>".to_vec());
        assert_eq!(
            [parts[0].clone(), parts[1].clone()].concat(),
            document,
            "splitting must lose no bytes"
        );
    }

    /// The ordinary case must keep working unchanged.
    #[test]
    fn after_marker_cuts_after_the_marker_in_valid_utf8() {
        let document = b"<Status><metadata/></Status>".to_vec();
        let parts = split(&document, &Chunking::AfterMarker("<metadata/>".to_string()));
        assert_eq!(
            parts,
            vec![b"<Status><metadata/>".to_vec(), b"</Status>".to_vec()]
        );
    }

    /// An absent marker keeps the whole-document fallback rather than splitting at nothing.
    #[test]
    fn an_absent_marker_yields_the_whole_document() {
        let document = b"<Status/>".to_vec();
        let parts = split(&document, &Chunking::AfterMarker("<metadata/>".to_string()));
        assert_eq!(parts, vec![document]);
    }

    /// A marker longer than the document is absent, not a panic. `slice::windows` yields nothing when
    /// the window exceeds the slice, which is the same answer `str::find` gave — pinned so the byte
    /// search cannot regress into an arithmetic overflow on a short buffer.
    #[test]
    fn a_marker_longer_than_the_document_is_simply_absent() {
        let document = b"<S/>".to_vec();
        let parts = split(
            &document,
            &Chunking::AfterMarker("a-very-long-marker".to_string()),
        );
        assert_eq!(parts, vec![document]);
    }

    /// An empty marker must not panic. `slice::windows(0)` panics, so the byte search needs the same
    /// "matches at offset 0" answer `str::find("")` gives — this is the case that turns the nitpick
    /// into a crash if implemented naively.
    #[test]
    fn an_empty_marker_matches_at_the_start_rather_than_panicking() {
        let document = b"<Status/>".to_vec();
        let parts = split(&document, &Chunking::AfterMarker(String::new()));
        assert_eq!(parts, vec![Vec::new(), document]);
    }
}
