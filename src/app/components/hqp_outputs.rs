//! Digital audio output routing (NAA/managed-relay destination selection) for the HQPlayer page.
//!
//! Wired against the backend's published contract in
//! `unified_hifi_control::adapters::hqplayer::outputs` (mirrored client-side in
//! `crate::app::api`). Owns client-side invariants the contract does not enforce on its own:
//! request-generation fencing so Stop always defeats a held switch AND a stale background
//! refresh, exact-instance command targeting read at call time, and rendering that never claims
//! audio is flowing without the contract's own evidence (a session in `forwarding` state, started,
//! positive current-stream bytes, and a matching `route_generation` — see
//! `session_confirms_audio`). Every render call site goes through the same tested functions this
//! file defines; there is no separate "helper" path that mirrors but doesn't drive what's shown.
//!
//! `resolve_command_target(move || instance())` appears throughout this file. `Signal<String>`
//! does not itself implement `Fn() -> String` (only `rsx!`'s own macro sugar can call it bare),
//! so the wrapping closure is required, not redundant, despite matching clippy's
//! `redundant_closure` shape heuristic.
#![allow(clippy::redundant_closure)]

use crate::app::api::{
    HqpOutputAction, HqpOutputAvailability, HqpOutputCommandRequest, HqpOutputOutcome,
    HqpOutputPhase, HqpOutputProjection, HqpRelaySessionView,
};

/// Fences stale responses out of the UI so a slow in-flight request cannot overwrite state a
/// later one already owns. Every async operation — command *and* background refresh alike — takes
/// a generation via `begin()`/`stop()` and checks `accept()` before applying its result. `stop()`
/// is a named alias for the same bump, used at Stop's call site so the intent (priority
/// cancellation, not just another queued request) is visible in the caller.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OutputCommandFence {
    generation: u64,
}

impl OutputCommandFence {
    pub fn begin(&mut self) -> u64 {
        self.generation += 1;
        self.generation
    }

    pub fn stop(&mut self) -> u64 {
        self.begin()
    }

    /// Returns `true` only if `generation` is still the newest issued operation; otherwise the
    /// caller must discard the response unread (never apply it, never surface its error).
    pub fn accept(&self, generation: u64) -> bool {
        generation == self.generation
    }
}

/// Resolves the exact-instance target for a command at the moment it is issued, not at the moment
/// the handler closure was constructed. Takes a getter rather than a value so callers are forced
/// to read the live signal instead of capturing a snapshot — see
/// `command_target_reflects_the_instance_at_call_time_not_bind_time` for the regression this
/// guards against (two instances open, switch active instance, a stale closure fires against the
/// instance that was selected when the button was rendered instead of the one showing now).
pub fn resolve_command_target(current_instance: impl Fn() -> String) -> String {
    let instance = current_instance();
    format!("hqplayer:{instance}")
}

/// Per-tab correlation id source. Deliberately not `uuid`: that crate is only enabled under the
/// `server` Cargo feature, not `web`, so calling it from shared frontend code would fail to link
/// the wasm32/browser build.
///
/// A bare in-memory counter is not enough on its own: it resets to 0 on every page reload, so a
/// Stop issued right after a reload could mint the same id ("ui-stop-0") as a Stop from the
/// *previous* page load. The backend deduplicates by correlation id AND request fingerprint —
/// Stop's fingerprint is always identical for a given zone (it has no other fields) — so a
/// same-id, same-fingerprint replay would return the backend's cached prior result instead of
/// executing a new Stop.
///
/// `session_salt_bytes()` draws 128 bits of real randomness. On wasm32, `getrandom` (declared
/// under `[target.'cfg(target_arch = "wasm32")'.dependencies]` in `Cargo.toml`, built with the
/// `js` feature for `rand`'s wasm32 backend) is backed by the browser's
/// `crypto.getRandomValues()` — safe over plain LAN HTTP, since `getRandomValues` has no
/// secure-context requirement, unlike `SubtleCrypto`. On the server target, `getrandom` is not a
/// direct dependency at all (only reachable transitively), so this uses `uuid::Uuid::new_v4()`
/// (backed by OS entropy) instead — a real UUID, not a formatted timestamp. One draw per page
/// load, cached in `SESSION_SALT`, mixed into every id so the counter restarting at 0 on reload
/// never reproduces a prior session's id.
#[cfg(target_arch = "wasm32")]
fn session_salt_bytes() -> [u8; 16] {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).expect("a secure random source is available");
    bytes
}

#[cfg(not(target_arch = "wasm32"))]
fn session_salt_bytes() -> [u8; 16] {
    uuid::Uuid::new_v4().into_bytes()
}

fn session_salt_hex() -> String {
    session_salt_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

static SESSION_SALT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
static CORRELATION_COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Pure formatting, factored out so the id-collision-across-sessions property is directly
/// testable without depending on the real random source.
fn format_correlation_id(purpose: &str, salt: &str, counter: u32) -> String {
    format!("ui-{purpose}-{salt}-{counter}")
}

fn new_correlation_id(purpose: &str) -> String {
    let salt = SESSION_SALT.get_or_init(session_salt_hex);
    let counter = CORRELATION_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format_correlation_id(purpose, salt, counter)
}

/// Builds the exact request body a mutation sends, echoing `expected_source_epoch` and
/// `expected_output_revision` from the most recently fetched projection when (and only when) the
/// action requires them. Centralizing this is what makes "selection succeeds while streaming
/// telemetry (`aggregate_revision`) keeps advancing" and "the echoed values always come from the
/// same live read, never a stale capture" true by construction rather than by convention at each
/// call site.
pub fn build_command(
    zone_id: String,
    correlation_id: Option<String>,
    action: HqpOutputAction,
    projection: Option<&HqpOutputProjection>,
) -> HqpOutputCommandRequest {
    let (expected_source_epoch, expected_output_revision) = if action.requires_expectations() {
        (
            projection.map(|p| p.source_epoch),
            projection.map(|p| p.output_revision),
        )
    } else {
        (None, None)
    };
    HqpOutputCommandRequest {
        zone_id,
        correlation_id,
        expected_source_epoch,
        expected_output_revision,
        action,
    }
}

/// Stop is disabled only when there is structurally nothing to cancel (the relay itself is
/// disabled in this instance's settings). It must never be disabled merely because a request is
/// already in flight — a held select is exactly the case Stop exists to cancel.
pub fn stop_button_disabled(availability: Option<&HqpOutputAvailability>) -> bool {
    matches!(availability, Some(HqpOutputAvailability::Disabled))
}

/// The backend defaults a blank/omitted route port to 43210 (contract: `port: Option<u16>
/// /*43210*/`). Any code that needs to know "which endpoint does this form actually describe" —
/// the DAC-observation lookup, in particular — must use this same effective value, or a blank
/// port field would wrongly read as "no observation" for an endpoint that actually has one at the
/// real (default) port the submit is about to use.
pub fn effective_port(form_port: Option<u16>) -> u16 {
    form_port.unwrap_or(43210)
}

/// The contract's audio-claim rule, session half: a session only backs an "audio is flowing"
/// claim when it reports `forwarding`, has actually `started`, has positive current-stream bytes,
/// and its `route_generation` matches the projection's current one (so a session left over from a
/// route that has since been superseded can never be read as confirming the *current* selection).
pub fn session_confirms_audio(
    session: &HqpRelaySessionView,
    current_route_generation: u64,
) -> bool {
    session.state == "forwarding"
        && session.route_generation == current_route_generation
        && session.started
        && session.current_stream_audio_bytes > 0
}

/// The single source of truth for whether this projection's live session backs an audio-flowing
/// claim. Requires, together: the relay itself reporting `Available` right now (a retained last
/// observation from an unavailable/disabled relay must never be read as current), the session's
/// route matching the *selected* route (a generation match alone does not prove it is the route
/// the operator currently has selected), and the session-level forwarding/started/bytes/generation
/// check. `None` session is never confirming.
pub fn audio_confirmed_for(projection: &HqpOutputProjection) -> bool {
    if !matches!(
        projection.availability(),
        Some(HqpOutputAvailability::Available)
    ) {
        return false;
    }
    let Some(session) = projection.session.as_ref() else {
        return false;
    };
    if projection.selected_route_id.as_deref() != Some(session.route_id.as_str()) {
        return false;
    }
    session_confirms_audio(session, projection.route_generation)
}

/// The exact text rendered for the current status line. Action-aware: reaching a confirming
/// phase (`Forwarding`/`Complete`) means something different depending on *what* completed — a
/// completed Stop, relay configuration save, discovery scan, route edit, or import/setup step
/// must never be described as "waiting for confirmed audio evidence", since none of those actions
/// claim anything about audio at all. `audio_confirmed` (computed once, from `audio_confirmed_for`)
/// is consulted *only* for `select`, the one action that actually attempts to change what's
/// forwarding — every other action's completion text is fixed and never mentions audio. `phase:
/// None` means no operation is currently tracked for this instance; it still defers to
/// `audio_confirmed` because a session can be genuinely forwarding with no operation in flight
/// (e.g. after a page reload), which is the one case this function is allowed to say "Forwarding
/// audio" without an accompanying `select` action.
pub fn render_status_label(
    action: Option<&str>,
    phase: Option<HqpOutputPhase>,
    audio_confirmed: bool,
) -> String {
    let confirming_phase = matches!(
        phase,
        Some(HqpOutputPhase::Forwarding) | Some(HqpOutputPhase::Complete)
    );
    if confirming_phase || phase.is_none() {
        // `select` (or no tracked action at all, e.g. straight after a page reload with nothing
        // locally issued) is the only case allowed to describe audio state.
        if action == Some("select") || action.is_none() {
            return if audio_confirmed {
                "Forwarding audio".to_string()
            } else if phase.is_some() {
                "Accepted — waiting for confirmed audio evidence".to_string()
            } else {
                "No pending operation".to_string()
            };
        }
        return match action {
            Some("stop") => "Stopped".to_string(),
            Some("relay_configure") => "Relay configuration saved".to_string(),
            Some("discover") => "Scan complete".to_string(),
            Some("route_add") => "Route added".to_string(),
            Some("route_update") => "Route updated".to_string(),
            Some("route_remove") => "Route removed".to_string(),
            Some("import_preview") => "Import preview ready".to_string(),
            Some("import_apply") => "Import applied".to_string(),
            Some("setup_preview") => "Setup preview ready".to_string(),
            Some("setup_apply") => "Setup applied".to_string(),
            Some("setup_readback") => "Configuration read back".to_string(),
            Some("setup_rollback") => "Rolled back".to_string(),
            _ => "Completed".to_string(),
        };
    }
    match phase {
        Some(HqpOutputPhase::Admitted) => "Requested…".to_string(),
        Some(HqpOutputPhase::Checking) => "Checking…".to_string(),
        Some(HqpOutputPhase::Stopping) => "Stopping previous output…".to_string(),
        Some(HqpOutputPhase::Committed) => "Committed…".to_string(),
        Some(HqpOutputPhase::Connecting) => "Connecting…".to_string(),
        Some(HqpOutputPhase::Initialized) => "Handshake accepted…".to_string(),
        Some(HqpOutputPhase::Resuming) => "Resuming…".to_string(),
        Some(HqpOutputPhase::Cancelled) => "Cancelled".to_string(),
        Some(HqpOutputPhase::Rejected) => "Rejected".to_string(),
        Some(HqpOutputPhase::Failed) => "Failed".to_string(),
        Some(HqpOutputPhase::Partial) => "Partially completed".to_string(),
        Some(HqpOutputPhase::Indeterminate) => {
            "Unknown outcome — a native write may not have completed; check the device".to_string()
        }
        Some(HqpOutputPhase::Unknown) => {
            "Unrecognized status — reload to check current state".to_string()
        }
        None | Some(HqpOutputPhase::Forwarding) | Some(HqpOutputPhase::Complete) => {
            unreachable!("both handled by the confirming_phase/None branch above, which returns")
        }
    }
}

/// Whether the discovery UDP responder is in a failed/unsupported state that the operator needs
/// to see. `discovery_responder: None` alongside `discovery_interface: None` is normal — discovery
/// was never configured. `discovery_responder: None` while an interface IS configured means the
/// responder failed to bind (or discovery is otherwise unsupported on this host) even though the
/// operator asked for it — a real listener failure, not the same as "discovery simply off".
pub fn discovery_responder_unsupported(
    discovery_interface: Option<&str>,
    discovery_responder: Option<&str>,
) -> bool {
    discovery_interface.is_some() && discovery_responder.is_none()
}

/// Copy for the unavailable-relay banner. Deliberately does not assert anything about whether
/// audio is or is not currently flowing: an unreachable relay is a failure to observe, not proof
/// of silence. Routes/observations shown alongside it are explicitly last-known, not current.
pub fn unavailable_banner_text(reason: &str) -> String {
    format!(
        "This instance's relay is unavailable ({reason}). Routes and observations below reflect \
         the last known state; current audio status cannot be confirmed from here."
    )
}

// =============================================================================
// Shared operation driver: every mutation (select, route CRUD, discover,
// relay_configure, stop, import, setup preview/apply/readback/rollback) goes through
// `drive_output_command` below, so there is exactly one place that posts a command,
// applies its accepted projection, and polls a non-terminal operation to completion.
// Transport-injectable (`OutputTransport`) specifically so this sequencing — bounded
// polling with backoff, reconciliation against a fresher aggregate read, fencing —
// is unit-testable against scripted, deferred responses instead of only through
// pure helper functions that don't exercise the actual async sequencing.
// =============================================================================

/// An operation is done exactly when the backend has set `outcome` — see
/// `HqpOutputOperation::is_terminal` in the backend crate, mirrored here since the frontend DTO
/// doesn't carry that inherent method.
fn terminal_or_none(op: &HqpOutputOperation) -> Option<HqpOutputOperation> {
    op.outcome.is_some().then(|| op.clone())
}

/// Prefers whichever of `polled` (this driver's own poll response) or `aggregate` (the same
/// operation id, if already present in the last-applied full projection — e.g. because an
/// SSE-triggered background refresh landed a fresher copy while this driver was mid-poll) is
/// actually more current: a terminal record always beats a non-terminal one regardless of
/// timestamps, and otherwise the later `updated_at` wins. This is what "reconcile fresher
/// aggregate operations instead of stale polled_operation priority" means concretely — a fresher
/// aggregate read must not lose to an older, already-in-flight poll's own response.
pub fn reconcile_operation(
    polled: Option<&HqpOutputOperation>,
    aggregate: Option<&HqpOutputOperation>,
) -> Option<HqpOutputOperation> {
    match (polled, aggregate) {
        (None, None) => None,
        (Some(p), None) => Some(p.clone()),
        (None, Some(a)) => Some(a.clone()),
        (Some(p), Some(a)) => {
            if a.outcome.is_some() && p.outcome.is_none() {
                Some(a.clone())
            } else if p.outcome.is_some() && a.outcome.is_none() {
                Some(p.clone())
            } else if a.updated_at >= p.updated_at {
                Some(a.clone())
            } else {
                Some(p.clone())
            }
        }
    }
}

/// Initial poll delay. Discovery is a bounded ~2s scan per contract; starting well under that
/// keeps a fast operation feeling immediate.
const POLL_INITIAL_DELAY_MS: u32 = 250;
/// Exponential backoff ceiling so a long-running operation (e.g. setup apply's upload) doesn't
/// hammer the server every quarter-second for minutes.
const POLL_MAX_DELAY_MS: u32 = 4_000;
/// Bounded attempts so a truly stuck operation eventually surfaces as an error the operator can
/// act on (via the manual Refresh button) instead of polling silently forever.
const POLL_MAX_ATTEMPTS: u32 = 30;
/// A transient fetch failure retries; this many *consecutive* failures gives up rather than
/// retrying indefinitely against a genuinely unreachable server.
const POLL_MAX_CONSECUTIVE_ERRORS: u32 = 4;

/// Injected so `drive_output_command`'s sequencing is testable against scripted, deferred
/// responses without a live HTTP client or a running Dioxus app. `HttpOutputTransport` is the
/// real implementation; tests substitute their own.
pub trait OutputTransport {
    fn post_command(
        &self,
        request: HqpOutputCommandRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<HqpOutputCommandReceipt, String>> + '_>,
    >;
    fn fetch_operation(
        &self,
        zone_id: String,
        operation_id: String,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<HqpOutputOperation>> + '_>>;
    fn sleep(&self, ms: u32) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + '_>>;
}

/// The outcome `drive_output_command` reports back to its Dioxus caller: whether this call was
/// superseded (a newer mutation or instance switch — caller must not touch busy/error state),
/// any error message to surface, and the terminal operation if one was reached.
pub struct DriveOutcome {
    pub superseded: bool,
    pub error: Option<String>,
    pub terminal: Option<HqpOutputOperation>,
}

/// The shared driver. Posts `request`, applies its accepted projection via `on_projection`
/// (unconditionally — every mutation must apply its receipt's projection so the *next* command's
/// echoed `expected_source_epoch`/`expected_output_revision` is never stale), publishes the
/// admission-time operation via `on_operation`, and — if it is not already terminal — polls
/// `fetch_operation` with exponential backoff until it is, reconciling each poll against
/// `aggregate_operation` (the caller's lookup into whatever the last-applied full projection
/// already knows about this operation id) so a fresher background read always wins over a stale
/// poll response. `still_current` is checked before posting is even acted on further and before
/// every poll step; the instant it returns `false` (a newer mutation, Stop, or an instance switch
/// tore down the caller's fence) this returns `superseded: true` immediately, touching no output
/// signals from that point on.
pub async fn drive_output_command<T: OutputTransport>(
    transport: &T,
    request: HqpOutputCommandRequest,
    still_current: impl Fn() -> bool,
    aggregate_operation: impl Fn(&str) -> Option<HqpOutputOperation>,
    mut on_projection: impl FnMut(HqpOutputProjection),
    mut on_operation: impl FnMut(HqpOutputOperation),
) -> DriveOutcome {
    let zone_id = request.zone_id.clone();
    let receipt = match transport.post_command(request).await {
        Ok(receipt) => receipt,
        Err(e) => {
            return DriveOutcome {
                superseded: !still_current(),
                error: Some(e),
                terminal: None,
            };
        }
    };
    on_projection(receipt.projection);
    if !still_current() {
        return DriveOutcome {
            superseded: true,
            error: None,
            terminal: None,
        };
    }
    on_operation(receipt.operation.clone());
    if let Some(done) = terminal_or_none(&receipt.operation) {
        return DriveOutcome {
            superseded: false,
            error: None,
            terminal: Some(done),
        };
    }

    let operation_id = receipt.operation.operation_id;
    let mut delay_ms = POLL_INITIAL_DELAY_MS;
    let mut consecutive_errors = 0u32;
    for _ in 0..POLL_MAX_ATTEMPTS {
        transport.sleep(delay_ms).await;
        if !still_current() {
            return DriveOutcome {
                superseded: true,
                error: None,
                terminal: None,
            };
        }
        match transport
            .fetch_operation(zone_id.clone(), operation_id.clone())
            .await
        {
            Some(polled) => {
                consecutive_errors = 0;
                if !still_current() {
                    return DriveOutcome {
                        superseded: true,
                        error: None,
                        terminal: None,
                    };
                }
                let reconciled =
                    reconcile_operation(Some(&polled), aggregate_operation(&operation_id).as_ref())
                        .unwrap_or(polled);
                on_operation(reconciled.clone());
                if let Some(done) = terminal_or_none(&reconciled) {
                    return DriveOutcome {
                        superseded: false,
                        error: None,
                        terminal: Some(done),
                    };
                }
            }
            None => {
                consecutive_errors += 1;
                if consecutive_errors >= POLL_MAX_CONSECUTIVE_ERRORS {
                    // Re-check before giving up: a newer mutation/Stop/instance switch may have
                    // superseded this call during the error-retry stretch itself. Without this
                    // check, a held operation that happens to be failing its polls right as Stop
                    // (or any newer mutation) supersedes it would report a normal error here —
                    // and the caller, seeing `superseded: false`, would apply that stale error
                    // (and clear `busy`) on top of whatever fresher state the newer mutation
                    // already established. That is the exact "held failing poll after Stop must
                    // not clear newer busy/error" regression this guards against.
                    return DriveOutcome {
                        superseded: !still_current(),
                        error: Some(
                            "Could not confirm this operation's outcome; use Refresh to check \
                             current state."
                                .to_string(),
                        ),
                        terminal: None,
                    };
                }
            }
        }
        delay_ms = delay_ms.saturating_mul(2).min(POLL_MAX_DELAY_MS);
    }
    // Same reasoning as the consecutive-errors give-up above: attempts can exhaust in the same
    // instant a newer mutation supersedes this one, and the caller must be told so rather than
    // handed a stale timeout error to apply over fresher state.
    DriveOutcome {
        superseded: !still_current(),
        error: Some(
            "This operation is taking longer than expected; use Refresh to check current state."
                .to_string(),
        ),
        terminal: None,
    }
}

#[cfg(target_arch = "wasm32")]
async fn sleep_ms(ms: u32) {
    gloo_timers::future::TimeoutFuture::new(ms).await;
}

#[cfg(not(target_arch = "wasm32"))]
async fn sleep_ms(ms: u32) {
    tokio::time::sleep(std::time::Duration::from_millis(u64::from(ms))).await;
}

struct HttpOutputTransport;

impl OutputTransport for HttpOutputTransport {
    fn post_command(
        &self,
        request: HqpOutputCommandRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<HqpOutputCommandReceipt, String>> + '_>,
    > {
        Box::pin(async move {
            api::post_json::<_, HqpOutputCommandReceipt>("/hqplayer/outputs/command", &request)
                .await
        })
    }

    fn fetch_operation(
        &self,
        zone_id: String,
        operation_id: String,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<HqpOutputOperation>> + '_>> {
        Box::pin(fetch_operation(zone_id, operation_id))
    }

    fn sleep(&self, ms: u32) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + '_>> {
        Box::pin(sleep_ms(ms))
    }
}

use dioxus::prelude::*;

use crate::app::api::{
    self, HqpOutputCommandReceipt, HqpOutputOperation, HqpOutputResult, HqpSetupPreview,
    HqpSetupTransaction,
};
use crate::app::sse::use_sse;

/// Digital audio output (NAA / managed relay) routing section for the HQPlayer page.
///
/// Selects which configured instance to show, then reads/mutates that exact instance's output
/// routing through the shared `/hqplayer/outputs*` surface — the same surface HTTP and MCP
/// clients use, so there is exactly one client-side call site for every mutation. Software
/// fixtures only: this never claims to select a physical household DAC on its own — it renders
/// whatever the aggregate projection reports and lets the operator choose among the routes/devices
/// the backend has actually observed.
#[derive(Clone, Debug, PartialEq)]
pub struct HqpOutputInstance {
    pub name: String,
    pub host: Option<String>,
    pub connected: bool,
    pub product: Option<String>,
    pub version: Option<String>,
}

#[component]
pub fn HqpOutputRoutingSection(instances: Vec<HqpOutputInstance>) -> Element {
    let mut selected_instance = use_signal(String::new);

    // Default to the first known instance once the list arrives; do not clobber an operator's
    // later selection on unrelated re-renders.
    let instances_for_default = instances.clone();
    use_effect(use_reactive!(|instances_for_default| {
        if selected_instance.peek().is_empty() {
            if let Some(first) = instances_for_default.first() {
                selected_instance.set(first.name.clone());
            }
        }
    }));

    if instances.is_empty() {
        return rsx! {};
    }

    rsx! {
        section { id: "hqp-outputs", class: "mb-8",
            div { class: "mb-4 max-w-3xl flex flex-wrap items-baseline justify-between gap-3",
                div {
                    h2 { class: "text-lg font-semibold", "Digital audio output" }
                    p { class: "mt-1 text-sm text-muted",
                        "Choose which network audio destination this HQPlayer instance forwards to. Selection is verified against the live engine before this page calls it switched."
                    }
                }
                if instances.len() > 1 {
                    label { class: "text-sm",
                        span { class: "sr-only", "Instance" }
                        select {
                            class: "input",
                            value: "{selected_instance}",
                            onchange: move |evt| selected_instance.set(evt.value()),
                            for instance in instances.iter() {
                                {
                                    let product = instance.product.as_deref().filter(|v| !v.is_empty()).unwrap_or("HQPlayer");
                                    let version = instance.version.as_deref().filter(|v| !v.is_empty()).unwrap_or("version unknown");
                                    let status = if instance.connected { "connected" } else { "offline" };
                                    rsx! {
                                        option {
                                            key: "{instance.name}",
                                            value: "{instance.name}",
                                            "{instance.name} — {product} {version} — {status}"
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if !selected_instance().is_empty() {
                HqpOutputRouting { key: "{selected_instance}", instance: selected_instance }
            }
        }
    }
}

async fn fetch_outputs(zone_id: String) -> Option<HqpOutputProjection> {
    let url = format!(
        "/hqplayer/outputs?zone_id={}",
        urlencoding::encode(&zone_id)
    );
    api::fetch_json::<HqpOutputProjection>(&url).await.ok()
}

/// One correlated read of a specific operation, independent of the full projection. Backs
/// `HttpOutputTransport::fetch_operation`, called repeatedly (with backoff) by
/// `drive_output_command`'s poll loop for as long as an accepted command's operation remains
/// non-terminal — the receipt's own embedded `operation` is only an admission-time snapshot.
async fn fetch_operation(zone_id: String, operation_id: String) -> Option<HqpOutputOperation> {
    let url = format!(
        "/hqplayer/outputs/operation?zone_id={}&operation_id={}",
        urlencoding::encode(&zone_id),
        urlencoding::encode(&operation_id)
    );
    api::fetch_json::<HqpOutputOperation>(&url).await.ok()
}

/// Whether `incoming` should replace `current` as the displayed projection. Compares the
/// server's own authoritative `(source_epoch, aggregate_revision)` pair — not local
/// request-issuance order. `read_fence` (see `refresh_outputs`/`run_output_command`) already orders
/// responses by which request THIS client issued most recently, but that alone trusts local
/// bookkeeping; this is the independent, data-level check that a delivered projection is not
/// actually older than what's already on screen, regardless of which request produced it.
/// `source_epoch` is primary (the aggregator's reliable-projection epoch, which resets on
/// reconnect-class events); `aggregate_revision` breaks ties within the same epoch and advances
/// on every commit, including pure telemetry, so it is a finer-grained freshness signal than
/// `output_revision` (which only bumps on mutations) for this specific purpose.
pub fn should_apply_projection(
    current: Option<&HqpOutputProjection>,
    incoming: &HqpOutputProjection,
) -> bool {
    match current {
        None => true,
        Some(current) => {
            (incoming.source_epoch, incoming.aggregate_revision)
                >= (current.source_epoch, current.aggregate_revision)
        }
    }
}

/// The exact guard `run_output_command`'s real `on_projection` closure calls to decide whether
/// this command's own accepted-receipt projection should be written at all. Extracted to a named
/// function — rather than an inline `.accept()` call at the closure site — specifically so the
/// production callsite and a test can share the identical call, instead of a test that only
/// reimplements the formula and would keep passing even if the callsite regressed to gate on the
/// wrong fence. Takes `mutation_fence` (never `read_fence`, which also advances on every
/// unrelated background refresh — see `refresh_outputs` — and would make this command's own,
/// still-valid projection look "stale" by fence alone whenever an unrelated refresh happened to
/// land first): only Stop, a newer mutation, or an instance switch advances `mutation_fence`, and
/// each of those really does mean this command's projection no longer belongs in view.
pub fn command_projection_should_apply(
    mutation_fence: Signal<OutputCommandFence>,
    mutation_generation: u64,
) -> bool {
    mutation_fence.read().accept(mutation_generation)
}

/// Applies a projection read (from either a background refresh or a command receipt) through
/// `should_apply_projection`. A `None` result (fetch failed) never overwrites existing good data
/// — a transient background read failure must not blank out a command's own just-applied state
/// (this is what "preserve receipt operation_id despite a stale background GET" means in
/// practice: the failing/older read simply never gets to overwrite anything).
fn apply_projection_read(
    mut outputs: Signal<Option<HqpOutputProjection>>,
    mut loaded_once: Signal<bool>,
    result: Option<HqpOutputProjection>,
) {
    match result {
        Some(projection) => {
            let apply = should_apply_projection(outputs.read().as_ref(), &projection);
            if apply {
                outputs.set(Some(projection));
            }
        }
        None => {
            if outputs.read().is_none() {
                outputs.set(None);
            }
        }
    }
    loaded_once.set(true);
}

/// Reload the projection for the currently-displayed instance. Fenced against `read_fence` only —
/// deliberately a *separate* counter from the one that governs command busy/error state (see
/// `run_output_command`). A background/SSE-triggered refresh must never supersede a pending command's
/// own generation: doing so on a shared counter used to strand a held Select's `busy`/`error`
/// signals forever whenever an unrelated refresh happened to land first. `read_fence` still
/// prevents a slow, stale GET from clobbering fresher data by issuance order; `apply_projection_read`
/// additionally guards by the data's own epoch/revision (see `should_apply_projection`).
fn refresh_outputs(
    instance: Signal<String>,
    outputs: Signal<Option<HqpOutputProjection>>,
    loaded_once: Signal<bool>,
    mut read_fence: Signal<OutputCommandFence>,
) {
    let zone_id = resolve_command_target(move || instance());
    let generation = read_fence.write().begin();
    spawn(async move {
        let result = fetch_outputs(zone_id).await;
        if !read_fence.read().accept(generation) {
            return;
        }
        apply_projection_read(outputs, loaded_once, result);
    });
}

/// Issue a mutation through the shared operation driver (`drive_output_command`) — the single
/// path every mutation in this component uses: select, route CRUD, discover, relay_configure,
/// import preview/apply, setup preview/apply/readback/rollback, and Stop (`priority: true`) all
/// call this, so there is exactly one place that posts, applies the accepted projection, tracks
/// the operation, and polls it to completion.
///
/// `priority` selects `OutputCommandFence::stop()` over `::begin()` on both fences — Stop's
/// priority-cancellation semantics, applied uniformly through the same driver rather than a
/// parallel code path. `on_terminal` receives the terminal operation so callers with a typed
/// result to extract (setup, import) can react; callers with nothing extra to do pass `|_| {}`.
///
/// Fencing: `mutation_fence` governs this call's own `busy`/`error`/operation-tracking
/// resolution *and* whether this call's own accepted projection gets written at all (via
/// `command_projection_should_apply`, called by the `on_projection` closure below) — bumped only
/// by other mutations/Stop/an instance switch, never by a background refresh. This call also
/// bumps `read_fence` (the same counter `refresh_outputs` uses) so any background refresh already
/// in flight *before* this command was issued is fenced out when it resolves, but `read_fence` is
/// deliberately **not** used to gate this call's own projection write — a command's receipt is
/// gated on `mutation_fence` only, so an unrelated SSE-triggered refresh landing between this
/// command's issuance and its response can never make the command's own, still-valid projection
/// look "stale" by fence alone (see the regression `command_projection_should_apply` fixed).
/// `apply_projection_read`'s `should_apply_projection` independently checks the response's own
/// epoch/revision, so a held receipt describing genuinely older data than what a fresher read
/// already displayed still won't win — that check is data-based, not fence-based.
/// `current_operation_id`/`polled_operation` are signals of their own (not derived from the
/// projection), so a stale/rejected background GET can never clear or replace them.
/// Non-terminal operations are polled with bounded exponential backoff
/// (`drive_output_command`/`POLL_MAX_ATTEMPTS`), reconciled against whatever the aggregate
/// projection already knows about the same operation id at each step, and immediately abandoned
/// — no further signal writes, no callback — the moment `mutation_fence` no longer matches this
/// call's own generation (a newer mutation, Stop, or an instance switch that tore this whole
/// component down).
#[allow(clippy::too_many_arguments)]
fn run_output_command<F>(
    request: HqpOutputCommandRequest,
    mut mutation_fence: Signal<OutputCommandFence>,
    mut read_fence: Signal<OutputCommandFence>,
    mut error: Signal<Option<String>>,
    mut busy: Signal<bool>,
    outputs: Signal<Option<HqpOutputProjection>>,
    loaded_once: Signal<bool>,
    mut current_operation_id: Signal<Option<String>>,
    mut polled_operation: Signal<Option<HqpOutputOperation>>,
    priority: bool,
    on_terminal: F,
) where
    F: FnOnce(HqpOutputOperation) + 'static,
{
    error.set(None);
    busy.set(true);
    let mutation_generation = if priority {
        mutation_fence.write().stop()
    } else {
        mutation_fence.write().begin()
    };
    // The bump itself is what matters here (it fences out any background refresh already in
    // flight before this command was issued — see `refresh_outputs`); the returned generation is
    // deliberately not captured for gating this command's own projection write, which uses
    // `mutation_fence` instead (see the `on_projection` closure below).
    if priority {
        read_fence.write().stop();
    } else {
        read_fence.write().begin();
    };
    spawn(async move {
        let transport = HttpOutputTransport;
        let still_current =
            move || command_projection_should_apply(mutation_fence, mutation_generation);
        let outcome = drive_output_command(
            &transport,
            request,
            still_current,
            move |operation_id: &str| {
                outputs.read().as_ref().and_then(|projection| {
                    projection
                        .operations
                        .iter()
                        .find(|op| op.operation_id == operation_id)
                        .cloned()
                })
            },
            move |projection| {
                if command_projection_should_apply(mutation_fence, mutation_generation) {
                    apply_projection_read(outputs, loaded_once, Some(projection));
                }
            },
            move |operation| {
                current_operation_id.set(Some(operation.operation_id.clone()));
                polled_operation.set(Some(operation));
            },
        )
        .await;

        if outcome.superseded {
            return;
        }
        if let Some(message) = outcome.error {
            error.set(Some(message));
        }
        busy.set(false);
        if let Some(terminal) = outcome.terminal {
            on_terminal(terminal);
        }
    });
}

#[component]
fn HqpOutputRouting(instance: Signal<String>) -> Element {
    let sse = use_sse();

    let outputs = use_signal(|| None::<HqpOutputProjection>);
    let loaded_once = use_signal(|| false);
    // Deliberately separate: see the doc comments on `refresh_outputs`/`run_output_command`.
    let read_fence = use_signal(OutputCommandFence::default);
    let mutation_fence = use_signal(OutputCommandFence::default);
    let busy = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);

    let mut editing_route_id = use_signal(|| None::<String>);
    let mut form_name = use_signal(String::new);
    let mut form_host = use_signal(String::new);
    let mut form_port = use_signal(|| Some(43210u16));
    let mut form_device = use_signal(String::new);

    // Tracked independently of the projection so a stale/rejected background GET can never clear
    // or replace it (see `run_output_command`'s doc comment).
    let current_operation_id = use_signal(|| None::<String>);
    let polled_operation = use_signal(|| None::<HqpOutputOperation>);

    let mut relay_enabled = use_signal(|| false);
    let mut relay_form_dirty = use_signal(|| false);
    let mut relay_bind = use_signal(String::new);
    let mut relay_hqp_allow = use_signal(String::new);
    let mut relay_discovery_interface = use_signal(String::new);
    // Ordinary UI default is 43210 (the standard NAA port), matching what the backend itself
    // defaults an omitted `discovery_port` to — the operator never has to know or type this
    // unless they actually need a non-standard port.
    let mut relay_discovery_port = use_signal(|| 43210u16);

    // Sync the relay-configure form from the live projection so it always reflects what's
    // actually configured, not a stale local default. Only the fields below sync; anything the
    // operator is actively editing is a fresh render's problem, matching this page's existing
    // `ConfigForm` sync pattern elsewhere.
    use_effect(move || {
        if !relay_form_dirty() {
            if let Some(p) = outputs() {
                relay_enabled.set(p.relay.enabled);
                relay_bind.set(p.relay.bind.clone().unwrap_or_default());
                relay_hqp_allow.set(p.relay.hqp_allow.join(", "));
                relay_discovery_interface
                    .set(p.relay.discovery_interface.clone().unwrap_or_default());
                relay_discovery_port.set(p.relay.discovery_port);
            }
        }
    });

    let mut setup_preview = use_signal(|| None::<HqpSetupPreview>);
    let mut setup_transaction = use_signal(|| None::<HqpSetupTransaction>);

    // Initial load and reload whenever the selected instance changes (`instance` is read inside
    // this effect, so switching instances re-subscribes it).
    use_effect(use_reactive!(|instance| {
        refresh_outputs(instance, outputs, loaded_once, read_fence);
    }));

    // The backend's existing HqpStateChanged SSE hint fires on every output commit; use it only
    // to trigger a re-read (the contract does not add a new SSE event type — the polled GET
    // remains authoritative and works even after a client reconnects).
    let event_count = sse.event_count;
    use_effect(use_reactive!(|instance| {
        let _ = event_count();
        if sse.should_refresh_hqp() {
            refresh_outputs(instance, outputs, loaded_once, read_fence);
        }
    }));

    // Stop is priority cancellation (`priority: true` below bumps both fences via `.stop()`
    // before building or sending anything else), so a held select/route response — or a stale
    // in-flight refresh — already in flight is fenced out the instant it lands, regardless of
    // network arrival order. Never gated on `busy`: a held operation is exactly what Stop exists
    // to cancel. Stop's own operation reaches a terminal outcome quickly and deterministically
    // (cancel + bounded native Stop), but it still goes through the same bounded poll as every
    // other mutation rather than trusting a single embedded receipt, in case it doesn't.
    let stop = move |_| {
        let zone_id = resolve_command_target(move || instance());
        let request = build_command(
            zone_id,
            Some(new_correlation_id("stop")),
            HqpOutputAction::Stop,
            None,
        );
        run_output_command(
            request,
            mutation_fence,
            read_fence,
            error,
            busy,
            outputs,
            loaded_once,
            current_operation_id,
            polled_operation,
            true,
            |_operation| {},
        );
    };

    let discover = move |_| {
        let zone_id = resolve_command_target(move || instance());
        let request = build_command(
            zone_id,
            Some(new_correlation_id("discover")),
            HqpOutputAction::Discover,
            None,
        );
        run_output_command(
            request,
            mutation_fence,
            read_fence,
            error,
            busy,
            outputs,
            loaded_once,
            current_operation_id,
            polled_operation,
            false,
            move |operation| {
                if operation.outcome == Some(HqpOutputOutcome::Complete) {
                    relay_form_dirty.set(false);
                }
            },
        );
    };

    let submit_route_form = move |_| {
        let zone_id = resolve_command_target(move || instance());
        let name = form_name();
        let host = form_host();
        let port = form_port();
        let device_id = {
            let value = form_device();
            (!value.trim().is_empty()).then_some(value)
        };
        let action = match editing_route_id() {
            Some(route_id) => HqpOutputAction::RouteUpdate {
                route_id,
                name,
                host,
                port,
                device_id,
            },
            None => HqpOutputAction::RouteAdd {
                name,
                host,
                port,
                device_id,
            },
        };
        let projection = outputs();
        let request = build_command(
            zone_id,
            Some(new_correlation_id("route-form")),
            action,
            projection.as_ref(),
        );
        editing_route_id.set(None);
        form_name.set(String::new());
        form_host.set(String::new());
        form_port.set(Some(43210));
        form_device.set(String::new());
        run_output_command(
            request,
            mutation_fence,
            read_fence,
            error,
            busy,
            outputs,
            loaded_once,
            current_operation_id,
            polled_operation,
            false,
            |_operation| {},
        );
    };

    // One-time relay lifecycle configuration. Requires the usual epoch/output_revision echo
    // (RelayConfigure is a mutation), so it goes through the same shared driver — and the same
    // fenced operation tracking — as every other mutation, not a bespoke fetch.
    let submit_relay_configure = move |_| {
        let zone_id = resolve_command_target(move || instance());
        let hqp_allow: Vec<String> = relay_hqp_allow()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let bind = {
            let value = relay_bind();
            (!value.trim().is_empty()).then_some(value)
        };
        let discovery_interface = {
            let value = relay_discovery_interface();
            (!value.trim().is_empty()).then_some(value)
        };
        let action = HqpOutputAction::RelayConfigure {
            enabled: relay_enabled(),
            bind,
            hqp_allow,
            discovery_interface,
            discovery_port: Some(relay_discovery_port()),
            adapter_name: None,
        };
        let projection = outputs();
        let request = build_command(
            zone_id,
            Some(new_correlation_id("relay-configure")),
            action,
            projection.as_ref(),
        );
        run_output_command(
            request,
            mutation_fence,
            read_fence,
            error,
            busy,
            outputs,
            loaded_once,
            current_operation_id,
            polled_operation,
            false,
            move |operation| {
                if operation.outcome == Some(HqpOutputOutcome::Complete) {
                    relay_form_dirty.set(false);
                }
            },
        );
    };

    // Setup preview/readback/apply/rollback all route through the same shared driver as every
    // other mutation now — each carries its own typed `HqpOutputResult` the operator must read
    // (proposed changes, upload/readback status, rollback availability), extracted in
    // `on_terminal` once the driver has actually confirmed the operation reached a terminal
    // outcome, not assumed present on the initial admission receipt.
    let start_setup_preview = move |_| {
        let zone_id = resolve_command_target(move || instance());
        let request = build_command(
            zone_id,
            Some(new_correlation_id("setup-preview")),
            HqpOutputAction::SetupPreview,
            None,
        );
        run_output_command(
            request,
            mutation_fence,
            read_fence,
            error,
            busy,
            outputs,
            loaded_once,
            current_operation_id,
            polled_operation,
            false,
            move |operation| match operation.result {
                Some(HqpOutputResult::SetupPreview(preview)) => {
                    setup_preview.set(Some(preview));
                }
                _ => error.set(Some(
                    "Setup preview did not return a preview result.".to_string(),
                )),
            },
        );
    };

    let readback_setup = move |_| {
        let zone_id = resolve_command_target(move || instance());
        let request = build_command(
            zone_id,
            Some(new_correlation_id("setup-readback")),
            HqpOutputAction::SetupReadback,
            None,
        );
        run_output_command(
            request,
            mutation_fence,
            read_fence,
            error,
            busy,
            outputs,
            loaded_once,
            current_operation_id,
            polled_operation,
            false,
            move |operation| match operation.result {
                Some(HqpOutputResult::Setup(transaction)) => {
                    setup_transaction.set(Some(transaction));
                }
                _ => error.set(Some(
                    "Setup readback did not return a transaction result.".to_string(),
                )),
            },
        );
    };

    let apply_setup = move |_| {
        let Some(preview) = setup_preview() else {
            return;
        };
        let zone_id = resolve_command_target(move || instance());
        let projection = outputs();
        let request = build_command(
            zone_id,
            Some(new_correlation_id("setup-apply")),
            HqpOutputAction::SetupApply {
                preview_id: preview.preview_id.clone(),
            },
            projection.as_ref(),
        );
        let mut setup_preview = setup_preview;
        run_output_command(
            request,
            mutation_fence,
            read_fence,
            error,
            busy,
            outputs,
            loaded_once,
            current_operation_id,
            polled_operation,
            false,
            move |operation| match operation.result {
                Some(HqpOutputResult::Setup(transaction)) => {
                    setup_transaction.set(Some(transaction));
                    setup_preview.set(None);
                }
                _ => error.set(Some(
                    "Setup apply did not return a transaction result.".to_string(),
                )),
            },
        );
    };

    let rollback_setup = move |_| {
        let zone_id = resolve_command_target(move || instance());
        let projection = outputs();
        let request = build_command(
            zone_id,
            Some(new_correlation_id("setup-rollback")),
            HqpOutputAction::SetupRollback,
            projection.as_ref(),
        );
        run_output_command(
            request,
            mutation_fence,
            read_fence,
            error,
            busy,
            outputs,
            loaded_once,
            current_operation_id,
            polled_operation,
            false,
            move |operation| match operation.result {
                Some(HqpOutputResult::Setup(transaction)) => {
                    setup_transaction.set(Some(transaction));
                }
                _ => error.set(Some(
                    "Setup rollback did not return a transaction result.".to_string(),
                )),
            },
        );
    };

    let data = outputs();
    let is_busy = busy();

    if !loaded_once() {
        return rsx! {
            div { class: "card p-4", aria_busy: "true", "Loading output routing…" }
        };
    }

    let Some(projection) = data else {
        return rsx! {
            div { class: "card p-4",
                p { class: "text-sm text-muted", "Output routing is not available for this instance yet." }
            }
        };
    };

    if !projection.is_well_formed() {
        return rsx! {
            div { class: "card p-4",
                p { class: "text-sm text-muted", "The server returned an unrecognized output-routing response. Try refreshing." }
            }
        };
    }

    let availability = projection.availability();
    // Tracked client-side (`current_operation_id`) takes priority over the projection's own
    // `current_operation_id`: the tracked signal survives a stale/rejected background GET
    // untouched (see `run_output_command`'s doc comment), while the projection-derived value
    // would silently revert to whatever an old, still-being-applied read last carried. Falls back
    // to the projection when nothing has been tracked yet (e.g. straight after a page load with
    // no local command issued this session).
    //
    // The operation record itself is reconciled the same way the driver's own poll loop
    // reconciles mid-flight (`reconcile_operation`): `polled_operation` (this component's last
    // poll response) vs. whatever the just-applied projection's own `operations[]` already knows
    // for the same id — a background SSE refresh can land a fresher or terminal copy between poll
    // ticks, and that fresher aggregate copy must win over a stale `polled_operation`, not the
    // other way around.
    let tracked_operation_id =
        current_operation_id().or_else(|| projection.current_operation_id.clone());
    let current_operation: Option<HqpOutputOperation> =
        tracked_operation_id.as_ref().and_then(|id| {
            let aggregate = projection
                .operations
                .iter()
                .find(|op| &op.operation_id == id);
            reconcile_operation(polled_operation().as_ref(), aggregate)
        });
    let phase = current_operation.as_ref().map(|op| op.phase);
    let action = current_operation.as_ref().map(|op| op.action.as_str());
    let audio_confirmed = audio_confirmed_for(&projection);
    let status_text = render_status_label(action, phase, audio_confirmed);
    let stop_disabled = stop_button_disabled(availability);

    rsx! {
        div { class: "card p-4 sm:p-5",
            if !projection.relay.enabled || projection.routes.is_empty() {
                div { class: "bg-primary/5 border border-primary/30 rounded-lg p-4 mb-4",
                    h2 { class: "text-base font-semibold m-0", "Route HQPlayer through UHC" }
                    p { class: "text-sm mt-1 mb-3",
                        "Set this up once. HQPlayer keeps using the HiPhi Router device; UHC handles the downstream DAC and lets you switch it without restarting HQPlayer."
                    }
                    ol { class: "text-sm list-decimal ml-5 space-y-1",
                        li { "Enable the relay and save the settings below." }
                        li { "Discover or add the NAA endpoint that owns your DAC." }
                        li { "Select a route, then start playback in HQPlayer." }
                    }
                }
            }
            if let Some(ref err) = error() {
                div { class: "bg-red-900/20 border border-red-500/50 rounded-lg p-3 mb-4",
                    p { class: "text-red-400 m-0 text-sm", "{err}" }
                }
            }

            match availability {
                Some(crate::app::api::HqpOutputAvailability::Unavailable { reason, .. }) => rsx! {
                    div { class: "bg-amber-900/20 border border-amber-500/50 rounded-lg p-3 mb-4",
                        p { class: "text-amber-400 m-0 text-sm", "{unavailable_banner_text(reason)}" }
                    }
                },
                Some(crate::app::api::HqpOutputAvailability::Disabled) => rsx! {
                    div { class: "bg-slate-900/20 border border-slate-500/50 rounded-lg p-3 mb-4",
                        p { class: "text-muted m-0 text-sm", "The managed relay is disabled for this instance. Enable it in setup to route audio." }
                    }
                },
                Some(crate::app::api::HqpOutputAvailability::Available) => rsx! {},
                None => rsx! {
                    div { class: "bg-amber-900/20 border border-amber-500/50 rounded-lg p-3 mb-4",
                        p { class: "text-amber-400 m-0 text-sm", "Relay availability is unknown from this response. Try refreshing." }
                    }
                },
            }

            div { class: "flex flex-wrap items-center gap-3 mb-4",
                span {
                    class: if audio_confirmed { "status-ok" } else { "text-muted" },
                    "{status_text}"
                }
                button {
                    class: "btn btn-ghost btn-sm ml-auto",
                    disabled: is_busy,
                    onclick: move |_| refresh_outputs(instance, outputs, loaded_once, read_fence),
                    "Refresh"
                }
                button {
                    class: "btn btn-outline btn-sm",
                    disabled: is_busy,
                    onclick: discover,
                    "Discover devices"
                }
                button {
                    class: "btn btn-ghost btn-sm",
                    disabled: stop_disabled,
                    onclick: stop,
                    "Stop"
                }
            }

            if let Some(selected_id) = projection.selected_route_id.as_ref() {
                if let Some(selected) = projection.routes.iter().find(|route| &route.route_id == selected_id) {
                    div { class: "rounded-lg border border-primary/40 bg-primary/5 p-3 mb-4",
                        p { class: "text-xs text-muted m-0", "Current output" }
                        p { class: "font-medium m-0 mt-1", "{selected.name}" }
                        p { class: "text-sm text-muted m-0", "{selected.host}:{selected.port}" }
                        if let Some(device) = selected.device_id.as_ref() {
                            p { class: "text-xs text-muted m-0 mt-1", "DAC: {device}" }
                        }
                        p { class: "text-xs m-0 mt-2", if audio_confirmed { "Audio confirmed at the relay." } else { "Selection saved; audio has not been confirmed yet." } }
                    }
                }
            }

            div { class: "mb-4 border-b border-subtle pb-4",
                h3 { class: "text-sm font-semibold mb-2", "1. Connect HQPlayer to UHC" }
                p { class: "text-xs text-muted mb-2",
                    "HQPlayer selects this relay once. UHC then forwards the authentication handshake, control messages, and audio to the route you choose below. PCM and DSD stay unchanged; NAA6 track metadata can be updated for the selected zone. Switching routes does not edit an HQPlayer profile or restart HQPlayer."
                }
                div { class: "grid grid-cols-1 sm:grid-cols-2 gap-3",
                    label { class: "flex items-center gap-2 text-sm",
                        input {
                            r#type: "checkbox",
                            checked: relay_enabled(),
                            onchange: move |evt| {
                                relay_form_dirty.set(true);
                                relay_enabled.set(evt.checked());
                            },
                        }
                        "Enabled"
                    }
                    input {
                        class: "input",
                        r#type: "text",
                        placeholder: "Bind address, e.g. 127.0.0.1:43210",
                        value: "{relay_bind}",
                        oninput: move |evt| {
                            relay_form_dirty.set(true);
                            relay_bind.set(evt.value());
                        },
                    }
                    input {
                        class: "input",
                        r#type: "text",
                        placeholder: "Allowed HQPlayer IPs, comma-separated (optional)",
                        value: "{relay_hqp_allow}",
                        oninput: move |evt| {
                            relay_form_dirty.set(true);
                            relay_hqp_allow.set(evt.value());
                        },
                    }
                    input {
                        class: "input",
                        r#type: "text",
                        placeholder: "Discovery interface IPv4 (optional; blank disables discovery)",
                        value: "{relay_discovery_interface}",
                        oninput: move |evt| {
                            relay_form_dirty.set(true);
                            relay_discovery_interface.set(evt.value());
                        },
                    }
                    label { class: "block",
                        span { class: "block text-xs text-muted mb-1", "Discovery UDP port" }
                        input {
                            class: "input",
                            r#type: "number",
                            placeholder: "43210",
                            value: "{relay_discovery_port}",
                            oninput: move |evt| {
                                relay_form_dirty.set(true);
                                if let Ok(port) = evt.value().parse() {
                                    relay_discovery_port.set(port);
                                }
                            },
                        }
                    }
                }
                if discovery_responder_unsupported(
                    projection.relay.discovery_interface.as_deref(),
                    projection.relay.discovery_responder.as_deref(),
                ) {
                    div { class: "bg-amber-900/20 border border-amber-500/50 rounded-lg p-3 mt-2",
                        p { class: "text-amber-400 m-0 text-sm",
                            "Discovery is configured on this interface, but the responder failed to bind — \"Discover devices\" above will not find this instance from HQPlayer. Check the interface address and try saving again."
                        }
                    }
                }
                div { class: "mt-3",
                    button {
                        class: "btn btn-primary btn-sm",
                        disabled: is_busy,
                        onclick: submit_relay_configure,
                        "Save relay configuration"
                    }
                }
            }

            div { class: "mb-4",
                h3 { class: "text-sm font-semibold mb-2", "2. Choose a DAC route" }
                if projection.routes.is_empty() {
                    p { class: "text-sm text-muted", "No routes configured yet. Add a discovered NAA endpoint or enter one below." }
                } else {
                    ul { class: "space-y-2",
                        for route in projection.routes.iter() {
                            {
                                let route_id = route.route_id.clone();
                                let route_id_select = route_id.clone();
                                let route_id_edit = route_id.clone();
                                let route_id_remove = route_id.clone();
                                let is_selected = projection.selected_route_id.as_deref() == Some(route.route_id.as_str());
                                let route_name = route.name.clone();
                                let route_host = route.host.clone();
                                let route_port = route.port;
                                let route_device = route.device_id.clone().unwrap_or_default();
                                rsx! {
                                    li {
                                        key: "{route.route_id}",
                                        class: if is_selected { "flex items-center justify-between gap-3 rounded-lg border border-primary/50 bg-primary/5 p-3" } else { "flex items-center justify-between gap-3 rounded-lg border border-subtle p-3" },
                                        div { class: "min-w-0",
                                            p { class: "text-sm font-medium truncate",
                                                "{route.name}"
                                                if is_selected { span { class: "badge badge-secondary ml-2", "Selected" } }
                                            }
                                            p { class: "text-xs text-muted truncate", "{route.host}:{route.port}" }
                                        }
                                        div { class: "flex items-center gap-2 shrink-0",
                                            button {
                                                class: "btn btn-primary btn-sm",
                                                disabled: is_busy || is_selected,
                                                onclick: move |_| {
                                                    let zone_id = resolve_command_target(move || instance());
                                                    let projection = outputs();
                                                    let request = build_command(
                                                        zone_id,
                                                        Some(new_correlation_id("select")),
                                                        HqpOutputAction::Select { route_id: route_id_select.clone() },
                                                        projection.as_ref(),
                                                    );
                                                    run_output_command(request, mutation_fence, read_fence, error, busy, outputs, loaded_once, current_operation_id, polled_operation, false, |_operation| {});
                                                },
                                                "Select"
                                            }
                                            button {
                                                class: "btn btn-ghost btn-sm",
                                                disabled: is_busy,
                                                onclick: move |_| {
                                                    editing_route_id.set(Some(route_id_edit.clone()));
                                                    form_name.set(route_name.clone());
                                                    form_host.set(route_host.clone());
                                                    form_port.set(Some(route_port));
                                                    form_device.set(route_device.clone());
                                                },
                                                "Edit"
                                            }
                                            button {
                                                class: "btn btn-ghost btn-sm",
                                                disabled: is_busy,
                                                onclick: move |_| {
                                                    let zone_id = resolve_command_target(move || instance());
                                                    let projection = outputs();
                                                    let request = build_command(
                                                        zone_id,
                                                        Some(new_correlation_id("route-remove")),
                                                        HqpOutputAction::RouteRemove { route_id: route_id_remove.clone() },
                                                        projection.as_ref(),
                                                    );
                                                    run_output_command(request, mutation_fence, read_fence, error, busy, outputs, loaded_once, current_operation_id, polled_operation, false, |_operation| {});
                                                },
                                                "Remove"
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            div { class: "mb-4",
                h3 { class: "text-sm font-semibold mb-2",
                    if editing_route_id().is_some() { "Edit route" } else { "Add a route" }
                }
                div { class: "grid grid-cols-1 sm:grid-cols-2 gap-3",
                    input {
                        class: "input",
                        r#type: "text",
                        placeholder: "Name",
                        value: "{form_name}",
                        oninput: move |evt| form_name.set(evt.value()),
                    }
                    input {
                        class: "input",
                        r#type: "text",
                        placeholder: "Host",
                        value: "{form_host}",
                        // Changing the host invalidates whatever device id was picked for the
                        // *previous* host — device ids are only meaningful scoped to their own
                        // host+port, so leaving the old value in place risks silently submitting
                        // a stale, invisible DAC id from a different endpoint.
                        oninput: move |evt| {
                            form_host.set(evt.value());
                            form_device.set(String::new());
                        },
                    }
                    input {
                        class: "input",
                        r#type: "number",
                        placeholder: "Port (defaults to 43210)",
                        value: "{form_port().map(|p| p.to_string()).unwrap_or_default()}",
                        // Same reasoning as the host handler above: the effective endpoint changed,
                        // so any previously-picked device id no longer applies.
                        oninput: move |evt| {
                            form_port.set(evt.value().parse().ok());
                            form_device.set(String::new());
                        },
                    }
                    {
                        // Per-host+port DAC picker: once this exact endpoint has been enumerated
                        // through a relayed session, offer its actual observed devices instead of
                        // a free-text field — device ids are only meaningful scoped to their own
                        // host+port (the same id can exist on two different endpoints). Uses
                        // `effective_port` (not the raw, possibly-blank `form_port()`) so a blank
                        // port field — which the backend defaults to 43210 on submit — looks up
                        // the observation for that same real, effective port rather than always
                        // reporting "no observation" whenever the field happens to be empty.
                        let host = form_host();
                        let port = effective_port(form_port());
                        let observed_devices = projection
                            .dac_observations
                            .iter()
                            .find(|obs| obs.host == host && obs.port == port);
                        match observed_devices {
                            Some(observation) if !observation.devices.is_empty() => rsx! {
                                select {
                                    class: "input",
                                    value: "{form_device}",
                                    onchange: move |evt| form_device.set(evt.value()),
                                    option { value: "", "(resolve automatically)" }
                                    for device in observation.devices.iter() {
                                        option {
                                            key: "{device.id}",
                                            value: "{device.id}",
                                            selected: form_device() == device.id,
                                            "{device.id} — {device.description}"
                                        }
                                    }
                                }
                            },
                            _ => rsx! {
                                input {
                                    class: "input",
                                    r#type: "text",
                                    placeholder: "Device id (optional; resolves the endpoint's sole output when blank)",
                                    value: "{form_device}",
                                    oninput: move |evt| form_device.set(evt.value()),
                                }
                            },
                        }
                    }
                }
                div { class: "flex items-center gap-2 mt-3",
                    button {
                        class: "btn btn-primary btn-sm",
                        disabled: is_busy || form_name().trim().is_empty() || form_host().trim().is_empty(),
                        onclick: submit_route_form,
                        if editing_route_id().is_some() { "Save changes" } else { "Add route" }
                    }
                    if editing_route_id().is_some() {
                        button {
                            class: "btn btn-ghost btn-sm",
                            onclick: move |_| {
                                editing_route_id.set(None);
                                form_name.set(String::new());
                                form_host.set(String::new());
                                form_port.set(Some(43210));
                                form_device.set(String::new());
                            },
                            "Cancel"
                        }
                    }
                }
            }

            div { class: "mb-4",
                h3 { class: "text-sm font-semibold mb-2", "Discovered NAA hosts" }
                match projection.discovery.as_ref() {
                    None => rsx! {
                        p { class: "text-sm text-muted", "Not yet scanned. Use \"Discover devices\" above." }
                    },
                    Some(discovery) if discovery.endpoints.is_empty() => rsx! {
                        p { class: "text-sm text-muted", "Scan completed and found no NAA hosts on the network." }
                    },
                    Some(discovery) => rsx! {
                        ul { class: "space-y-1",
                            for endpoint in discovery.endpoints.iter() {
                                {
                                    let endpoint_name = endpoint.name.clone();
                                    let endpoint_host = endpoint.host.clone();
                                    let endpoint_port = endpoint.port;
                                    // A single NAA endpoint can expose more than one DAC, so a
                                    // route already existing at this host:port must never block
                                    // adding a *second* route there with a different device id —
                                    // never disable on host:port collision alone.
                                    let existing_route_count = projection
                                        .routes
                                        .iter()
                                        .filter(|route| route.host == endpoint.host && route.port == endpoint.port)
                                        .count();
                                    rsx! {
                                        li {
                                            key: "{endpoint.host}:{endpoint.port}",
                                            class: "text-sm flex items-center justify-between gap-3",
                                            span {
                                                "{endpoint.name} — {endpoint.host}:{endpoint.port} ({endpoint.protocol})"
                                                if existing_route_count > 0 {
                                                    span { class: "text-xs text-muted ml-2",
                                                        "({existing_route_count} route(s) already configured here)"
                                                    }
                                                }
                                            }
                                            button {
                                                class: "btn btn-ghost btn-sm shrink-0",
                                                disabled: is_busy,
                                                title: "Fill in the route form below with this host — pick a different device id for a second DAC on the same endpoint",
                                                onclick: move |_| {
                                                    editing_route_id.set(None);
                                                    form_name.set(endpoint_name.clone());
                                                    form_host.set(endpoint_host.clone());
                                                    form_port.set(Some(endpoint_port));
                                                    form_device.set(String::new());
                                                },
                                                "Add as route"
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    },
                }
            }

            details { class: "mb-4",
                summary { class: "text-sm font-semibold cursor-pointer", "Inspect discovered hosts and DACs" }
                p { class: "text-xs text-muted mb-2",
                    "Per endpoint (host:port), from the last relayed authenticated session. An endpoint with no entry here has never been enumerated — that is different from one that enumerated zero outputs."
                }
                if projection.dac_observations.is_empty() {
                    p { class: "text-sm text-muted", "No endpoint has been enumerated yet. Select a route to establish a relayed session." }
                } else {
                    ul { class: "space-y-2",
                        for observation in projection.dac_observations.iter() {
                            li { key: "{observation.host}:{observation.port}", class: "text-sm",
                                p { class: "font-medium", "{observation.host}:{observation.port}" }
                                p { class: "text-xs text-muted", "last seen {observation.observed_at}" }
                                if observation.devices.is_empty() {
                                    p { class: "text-xs text-muted", "This endpoint reported zero outputs." }
                                } else {
                                    ul { class: "ml-4 list-disc",
                                        for device in observation.devices.iter() {
                                            li { key: "{device.id}", class: "text-xs", "{device.id} — {device.description}" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            details { class: "mb-4",
                summary { class: "text-sm font-semibold cursor-pointer", "Advanced: one-time HQPlayer setup" }
                p { class: "text-xs text-muted mb-2",
                    "Derives the proposed <output> change from this relay's own configuration and HQPlayer's current backup. Preview never uploads anything; a prior applied change can be rolled back to what was there before."
                }
                div { class: "flex flex-wrap items-center gap-2 mb-2",
                    button {
                        class: "btn btn-outline btn-sm",
                        disabled: is_busy,
                        onclick: start_setup_preview,
                        "Preview setup"
                    }
                    button {
                        class: "btn btn-ghost btn-sm",
                        disabled: is_busy,
                        onclick: readback_setup,
                        "Read back current configuration"
                    }
                }
                if let Some(preview) = setup_preview() {
                    div { class: "text-sm border border-subtle rounded-lg p-3 mb-2",
                        if !preview.applicable {
                            p { class: "text-amber-400 m-0",
                                "Not applicable: {preview.blocker.clone().unwrap_or_else(|| \"unknown reason\".to_string())}"
                            }
                        } else if preview.changes.is_empty() {
                            p { class: "text-muted m-0", "No changes needed — the current configuration already matches." }
                        } else {
                            p { class: "font-medium mb-1", "Proposed changes" }
                            ul { class: "list-disc ml-4",
                                for change in preview.changes.iter() {
                                    li { key: "{change.attribute}", class: "text-xs",
                                        "{change.attribute}: {change.from.clone().unwrap_or_else(|| \"(unset)\".to_string())} → {change.to}"
                                    }
                                }
                            }
                            button {
                                class: "btn btn-primary btn-sm mt-2",
                                disabled: is_busy,
                                onclick: apply_setup,
                                "Apply setup"
                            }
                        }
                    }
                }
                if let Some(transaction) = setup_transaction() {
                    div { class: "text-sm border border-subtle rounded-lg p-3",
                        p { class: "m-0", "Step: {transaction.step} — uploaded: {transaction.uploaded}" }
                        if let Some(daemon_response) = transaction.daemon_response.as_ref() {
                            p { class: "text-xs text-muted mt-1", "HQPlayer responded: {daemon_response}" }
                        }
                        if let Some(matches) = transaction.readback_matches {
                            p {
                                class: if matches { "status-ok mt-1" } else { "text-red-400 mt-1" },
                                if matches { "Readback confirms an exact byte match." } else { "Readback does NOT match — the configuration may not have applied as expected." }
                            }
                        }
                        if transaction.rollback_available {
                            button {
                                class: "btn btn-ghost btn-sm mt-2",
                                disabled: is_busy,
                                onclick: rollback_setup,
                                "Rollback to the pre-apply configuration"
                            }
                        }
                    }
                }
            }

        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::api::{HqpOutputAvailability, HqpOutputPhase, HqpRelaySessionView};
    use std::cell::RefCell;

    fn session(
        state: &str,
        route_generation: u64,
        started: bool,
        bytes: u64,
    ) -> HqpRelaySessionView {
        HqpRelaySessionView {
            session_id: 1,
            route_id: "r1".to_string(),
            route_generation,
            state: state.to_string(),
            peer: "127.0.0.1:1".to_string(),
            connected_at: 0,
            bytes_to_naa: 0,
            bytes_from_naa: 0,
            initialized: true,
            started,
            current_stream_audio_bytes: bytes,
        }
    }

    #[test]
    fn session_confirms_audio_requires_forwarding_state_started_and_bytes() {
        assert!(session_confirms_audio(
            &session("forwarding", 7, true, 100),
            7
        ));
        assert!(!session_confirms_audio(
            &session("connecting", 7, true, 100),
            7
        ));
        assert!(!session_confirms_audio(
            &session("forwarding", 7, false, 100),
            7
        ));
        assert!(!session_confirms_audio(
            &session("forwarding", 7, true, 0),
            7
        ));
    }

    #[test]
    fn session_confirms_audio_rejects_a_session_from_a_superseded_route_generation() {
        // A session left over from a route that has since been changed (route_generation bumped
        // by a later select/route mutation) must never confirm audio for the *current* selection,
        // even if it is still technically reporting forwarding+bytes for its own old generation.
        let stale_session = session("forwarding", 6, true, 4096);
        let current_route_generation = 7;
        assert!(!session_confirms_audio(
            &stale_session,
            current_route_generation
        ));
    }

    #[test]
    fn render_status_label_never_claims_forwarding_without_confirmed_audio_for_select() {
        // This is the exact regression: OutputPhase::label() used to say "Forwarding audio"
        // unconditionally for the Forwarding phase. render_status_label is now the only function
        // that produces this text, and it must refuse the claim without evidence — for `select`,
        // the one action this claim is even about.
        assert_eq!(
            render_status_label(Some("select"), Some(HqpOutputPhase::Forwarding), false),
            "Accepted — waiting for confirmed audio evidence"
        );
        assert_eq!(
            render_status_label(Some("select"), Some(HqpOutputPhase::Complete), false),
            "Accepted — waiting for confirmed audio evidence"
        );
    }

    #[test]
    fn render_status_label_claims_forwarding_only_with_confirmed_audio() {
        assert_eq!(
            render_status_label(Some("select"), Some(HqpOutputPhase::Forwarding), true),
            "Forwarding audio"
        );
        assert_eq!(
            render_status_label(Some("select"), Some(HqpOutputPhase::Complete), true),
            "Forwarding audio"
        );
    }

    #[test]
    fn render_status_label_reflects_a_live_session_even_with_no_tracked_operation() {
        // After a page reload there may be no current_operation_id at all, but a session can
        // genuinely already be forwarding. The absence of an operation must not suppress a real,
        // evidenced claim, and must not fabricate one either.
        assert_eq!(render_status_label(None, None, true), "Forwarding audio");
        assert_eq!(
            render_status_label(None, None, false),
            "No pending operation"
        );
    }

    #[test]
    fn render_status_label_reports_unrecognized_phases_distinctly_from_no_operation() {
        assert_eq!(
            render_status_label(Some("select"), Some(HqpOutputPhase::Unknown), false),
            "Unrecognized status — reload to check current state"
        );
        assert_ne!(
            render_status_label(Some("select"), Some(HqpOutputPhase::Unknown), false),
            render_status_label(None, None, false),
            "an unrecognized phase value must not be silently reported the same as no operation at all"
        );
    }

    #[test]
    fn render_status_label_is_action_aware_for_completed_non_audio_mutations() {
        // The exact regression this round fixes: a completed Stop, relay configuration save,
        // discovery scan, or route edit must never say "waiting for confirmed audio evidence" —
        // none of those actions claim anything about audio at all.
        let cases: &[(&str, &str)] = &[
            ("stop", "Stopped"),
            ("relay_configure", "Relay configuration saved"),
            ("discover", "Scan complete"),
            ("route_add", "Route added"),
            ("route_update", "Route updated"),
            ("route_remove", "Route removed"),
            ("import_preview", "Import preview ready"),
            ("import_apply", "Import applied"),
            ("setup_preview", "Setup preview ready"),
            ("setup_apply", "Setup applied"),
            ("setup_readback", "Configuration read back"),
            ("setup_rollback", "Rolled back"),
        ];
        for (action, expected) in cases {
            for audio_confirmed in [false, true] {
                let label = render_status_label(
                    Some(action),
                    Some(HqpOutputPhase::Complete),
                    audio_confirmed,
                );
                assert_eq!(
                    label, *expected,
                    "action {action}, audio_confirmed {audio_confirmed}"
                );
                assert!(
                    !label.to_lowercase().contains("audio"),
                    "action {action} must never mention audio in its completion text: {label}"
                );
            }
        }
    }

    #[test]
    fn render_status_label_still_uses_audio_evidence_only_for_select() {
        assert_eq!(
            render_status_label(Some("select"), Some(HqpOutputPhase::Complete), true),
            "Forwarding audio"
        );
        assert_eq!(
            render_status_label(Some("stop"), Some(HqpOutputPhase::Complete), true),
            "Stopped",
            "a completed Stop must say Stopped even if the session happens to still report \
             confirmed audio from some other route — Stop's own completion text never varies \
             with audio state"
        );
    }

    #[test]
    fn stop_is_never_disabled_by_an_in_flight_request() {
        // stop_button_disabled takes availability only — there is no busy/in-flight parameter at
        // all, which is the structural fix: a caller cannot wire "disabled: is_busy" back in
        // without changing this function's signature.
        assert!(!stop_button_disabled(Some(
            &HqpOutputAvailability::Available
        )));
        assert!(!stop_button_disabled(None));
    }

    #[test]
    fn stop_is_disabled_only_when_the_relay_itself_is_disabled() {
        assert!(stop_button_disabled(Some(&HqpOutputAvailability::Disabled)));
        assert!(!stop_button_disabled(Some(
            &HqpOutputAvailability::Unavailable {
                reason: "x".to_string(),
                since: 0,
            }
        )));
    }

    #[test]
    fn effective_port_defaults_a_blank_field_to_the_same_port_the_backend_submits() {
        // The DAC picker must look up the observation for the port that will actually be sent
        // (the backend defaults a blank/omitted port to 43210), not literally "no port" — or the
        // picker would wrongly report "no observation" for an endpoint it actually knows about.
        assert_eq!(effective_port(None), 43210);
        assert_eq!(effective_port(Some(60002)), 60002);
    }

    #[test]
    fn discovery_responder_unsupported_distinguishes_never_configured_from_failed_to_bind() {
        // Never configured at all: not a failure, nothing to warn about.
        assert!(!discovery_responder_unsupported(None, None));
        // Configured and actually bound: healthy.
        assert!(!discovery_responder_unsupported(
            Some("192.0.2.10"),
            Some("192.0.2.10:43210")
        ));
        // Configured but the responder never bound: a real listener failure the operator must see.
        assert!(discovery_responder_unsupported(Some("192.0.2.10"), None));
    }

    #[test]
    fn stop_defeats_a_held_switch_response_even_if_the_switch_was_issued_first() {
        let mut fence = OutputCommandFence::default();

        let select_generation = fence.begin();
        // Network holds the select response open ("held switch") while the user hits Stop.
        let stop_generation = fence.stop();

        assert_ne!(select_generation, stop_generation);
        assert!(
            !fence.accept(select_generation),
            "a held select response must be discarded once Stop has been issued"
        );
        assert!(
            fence.accept(stop_generation),
            "Stop's own response is still the current generation"
        );
    }

    #[test]
    fn stop_also_fences_out_a_stale_background_refresh_not_just_commands() {
        let mut fence = OutputCommandFence::default();
        let refresh_generation = fence.begin(); // e.g. an SSE-triggered refresh already in flight
        let stop_generation = fence.stop();
        assert_ne!(refresh_generation, stop_generation);
        assert!(
            !fence.accept(refresh_generation),
            "a refresh started before Stop must not be allowed to overwrite post-Stop state"
        );
    }

    #[test]
    fn a_fresh_command_after_stop_settles_is_accepted_normally() {
        let mut fence = OutputCommandFence::default();
        let _select_generation = fence.begin();
        let _stop_generation = fence.stop();

        let next_select = fence.begin();
        assert!(fence.accept(next_select));
    }

    #[test]
    fn an_unrelated_background_refresh_cannot_strand_a_held_commands_busy_or_error_state() {
        // This is the exact regression: when refresh and command shared one fence, an
        // SSE-triggered background refresh landing while a Select was in flight would bump the
        // shared counter, so the Select's own eventual response was discarded as "stale" —
        // leaving `busy` stuck true and `error` never set from the Select's real outcome.
        // `mutation_fence` and `read_fence` (used by `run_output_command`/`refresh_outputs`
        // respectively in the real component) are independent instances precisely so this cannot
        // happen: a background refresh only ever touches `read_fence`.
        let mut mutation_fence = OutputCommandFence::default();
        let mut read_fence = OutputCommandFence::default();

        let select_mutation_generation = mutation_fence.begin();
        // Unrelated background refresh runs concurrently, touching only read_fence.
        let _unrelated_refresh_generation = read_fence.begin();
        let _another_unrelated_refresh_generation = read_fence.begin();

        assert!(
            mutation_fence.accept(select_mutation_generation),
            "background refreshes on a separate fence must never affect a command's own \
             busy/error resolution"
        );
    }

    #[test]
    fn stop_invalidates_both_a_held_command_and_a_stale_background_read() {
        let mut mutation_fence = OutputCommandFence::default();
        let mut read_fence = OutputCommandFence::default();

        let select_mutation_generation = mutation_fence.begin();
        let select_read_generation = read_fence.begin();
        let stale_refresh_read_generation = read_fence.begin(); // e.g. an SSE refresh, still in flight

        let stop_mutation_generation = mutation_fence.stop();
        let stop_read_generation = read_fence.stop();

        assert!(!mutation_fence.accept(select_mutation_generation));
        assert!(!read_fence.accept(select_read_generation));
        assert!(!read_fence.accept(stale_refresh_read_generation));
        assert!(mutation_fence.accept(stop_mutation_generation));
        assert!(read_fence.accept(stop_read_generation));
    }

    #[test]
    fn command_target_reflects_the_instance_at_call_time_not_bind_time() {
        let current = RefCell::new("living-room".to_string());
        let target = resolve_command_target(|| current.borrow().clone());
        assert_eq!(target, "hqplayer:living-room");

        // Simulate the user switching the active instance after the handler was built but before
        // it is invoked again — the same shape as a closure captured once at component mount.
        *current.borrow_mut() = "office".to_string();
        let target_after_switch = resolve_command_target(|| current.borrow().clone());
        assert_eq!(
            target_after_switch, "hqplayer:office",
            "a command built after switching instances must target the new instance, not a stale capture"
        );
    }

    #[test]
    fn correlation_ids_from_different_sessions_do_not_collide_at_the_same_counter_value() {
        // Simulates the exact regression: a page reload resets the in-memory counter to 0, so
        // without a session salt, the first Stop after reload ("ui-stop-<salt>-0") could collide
        // with the first Stop's id from the *previous* session. Since Stop's request fingerprint
        // is always identical for a given zone, a colliding (correlation_id, fingerprint) pair
        // would make the backend return its cached prior result instead of executing a new Stop.
        let session_a_first_id = format_correlation_id("stop", "aaaaaaaa", 0);
        let session_b_first_id = format_correlation_id("stop", "bbbbbbbb", 0);
        assert_ne!(
            session_a_first_id, session_b_first_id,
            "two different session salts at the same reset counter value must not produce the same id"
        );
    }

    #[test]
    fn correlation_ids_are_unique_within_a_session() {
        let a = new_correlation_id("select");
        let b = new_correlation_id("select");
        assert_ne!(
            a, b,
            "two commands issued in the same session must not share a correlation id"
        );
    }

    /// **Tests the generator, not the formatter.** `format_correlation_id`'s own tests (above)
    /// only prove the string layout; this proves the actual entropy source draws real, distinct
    /// randomness on every call rather than something deterministic disguised as random.
    #[test]
    fn session_salt_bytes_draws_from_a_real_random_source_on_every_call() {
        let a = session_salt_bytes();
        let b = session_salt_bytes();
        assert_ne!(
            a, b,
            "two independent draws from the random source must not collide"
        );
        assert_ne!(
            a, [0u8; 16],
            "must not silently fall back to a zeroed buffer"
        );
        assert_ne!(
            b, [0u8; 16],
            "must not silently fall back to a zeroed buffer"
        );
    }

    #[test]
    fn session_salt_hex_is_128_bits_hex_encoded() {
        let salt = session_salt_hex();
        assert_eq!(salt.len(), 32, "16 bytes hex-encoded is 32 hex characters");
        assert!(salt.chars().all(|c| c.is_ascii_hexdigit()));
    }

    fn fixture_projection(
        source_epoch: u64,
        output_revision: u64,
        aggregate_revision: u64,
    ) -> HqpOutputProjection {
        HqpOutputProjection {
            zone_id: "hqplayer:living".to_string(),
            instance: "living".to_string(),
            source_epoch,
            aggregate_revision,
            output_revision,
            route_generation: 1,
            availability: crate::app::api::HqpOutputAvailabilityOrUnknown::Known(
                HqpOutputAvailability::Available,
            ),
            relay: Default::default(),
            routes: vec![],
            selected_route_id: None,
            desired_destination: None,
            observed_forwarding_destination: None,
            session: None,
            discovery: None,
            dac_observations: vec![],
            native: Default::default(),
            current_operation_id: None,
            operations: vec![],
            last_error: None,
            observed_at: 0,
        }
    }

    #[test]
    fn build_command_echoes_source_epoch_and_output_revision_for_mutations_that_need_them() {
        let projection = fixture_projection(3, 12, 418);
        let command = build_command(
            "hqplayer:living".to_string(),
            Some("corr-1".to_string()),
            HqpOutputAction::Select {
                route_id: "b77e".to_string(),
            },
            Some(&projection),
        );
        assert_eq!(command.expected_source_epoch, Some(3));
        assert_eq!(command.expected_output_revision, Some(12));
    }

    #[test]
    fn build_command_omits_expectations_for_stop_discover_and_preview_actions() {
        let projection = fixture_projection(3, 12, 418);
        for action in [
            HqpOutputAction::Stop,
            HqpOutputAction::Discover,
            HqpOutputAction::SetupPreview,
            HqpOutputAction::SetupReadback,
        ] {
            let command = build_command(
                "hqplayer:living".to_string(),
                None,
                action,
                Some(&projection),
            );
            assert_eq!(command.expected_source_epoch, None);
            assert_eq!(command.expected_output_revision, None);
        }
    }

    #[test]
    fn a_selection_during_ongoing_stream_telemetry_still_echoes_the_unchanged_output_revision() {
        // aggregate_revision advances on every streaming telemetry tick; output_revision only
        // advances on a real mutation. A selection issued while telemetry is actively updating
        // must echo output_revision as it stood in the projection actually read — not be thrown
        // off by aggregate_revision moving underneath it.
        let mid_stream_projection = fixture_projection(3, 12, 9001);
        let command = build_command(
            "hqplayer:living".to_string(),
            None,
            HqpOutputAction::Select {
                route_id: "r1".to_string(),
            },
            Some(&mid_stream_projection),
        );
        assert_eq!(
            command.expected_output_revision,
            Some(12),
            "must echo output_revision, not aggregate_revision, regardless of how far telemetry has advanced"
        );
    }

    #[test]
    fn build_command_with_no_projection_yet_sends_no_expectation_rather_than_inventing_one() {
        let command = build_command(
            "hqplayer:living".to_string(),
            None,
            HqpOutputAction::Select {
                route_id: "r1".to_string(),
            },
            None,
        );
        assert_eq!(command.expected_source_epoch, None);
        assert_eq!(command.expected_output_revision, None);
    }
}

/// Tests the real deferred asynchronous sequencing of `drive_output_command` against an
/// injectable `OutputTransport` — scripted, deferred responses, not just the pure helper
/// functions (`reconcile_operation`, `terminal_or_none`) those helpers alone would not exercise
/// the actual polling/backoff/fencing control flow, only their own isolated logic.
#[cfg(test)]
mod operation_driver_tests {
    use super::*;
    use crate::app::api::{HqpOutputEvidence, HqpOutputOutcome, HqpSetupPreview};
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::future::Future;
    use std::pin::Pin;
    use std::rc::Rc;

    fn op(
        operation_id: &str,
        action: &str,
        phase: HqpOutputPhase,
        outcome: Option<HqpOutputOutcome>,
        updated_at: u64,
        result: Option<HqpOutputResult>,
    ) -> HqpOutputOperation {
        HqpOutputOperation {
            operation_id: operation_id.to_string(),
            correlation_id: None,
            request_fingerprint: "fp".to_string(),
            zone_id: "hqplayer:living".to_string(),
            action: action.to_string(),
            route_id: None,
            phase,
            outcome,
            source_epoch: 1,
            output_revision_at_admission: 1,
            route_generation: None,
            admitted_at: 0,
            updated_at,
            detail: None,
            result,
            evidence: HqpOutputEvidence::default(),
        }
    }

    fn fixture_projection(
        source_epoch: u64,
        output_revision: u64,
        aggregate_revision: u64,
    ) -> HqpOutputProjection {
        HqpOutputProjection {
            zone_id: "hqplayer:living".to_string(),
            instance: "living".to_string(),
            source_epoch,
            aggregate_revision,
            output_revision,
            route_generation: 1,
            availability: crate::app::api::HqpOutputAvailabilityOrUnknown::Known(
                crate::app::api::HqpOutputAvailability::Available,
            ),
            relay: Default::default(),
            routes: vec![],
            selected_route_id: None,
            desired_destination: None,
            observed_forwarding_destination: None,
            session: None,
            discovery: None,
            dac_observations: vec![],
            native: Default::default(),
            current_operation_id: None,
            operations: vec![],
            last_error: None,
            observed_at: 0,
        }
    }

    fn receipt(
        operation: HqpOutputOperation,
        projection: HqpOutputProjection,
    ) -> HqpOutputCommandReceipt {
        HqpOutputCommandReceipt {
            accepted: true,
            operation,
            projection,
        }
    }

    fn empty_request(action: HqpOutputAction) -> HqpOutputCommandRequest {
        HqpOutputCommandRequest {
            zone_id: "hqplayer:living".to_string(),
            correlation_id: None,
            expected_source_epoch: None,
            expected_output_revision: None,
            action,
        }
    }

    /// Scripted transport: each call pops the next canned response and panics if the script runs
    /// out, so an unexpectedly-extra post/poll fails the test loudly instead of silently. `sleep`
    /// records the requested backoff delay and resolves immediately — this tests the actual
    /// sequencing/backoff *decisions* without real wall-clock waits.
    struct ScriptedTransport {
        post_responses: RefCell<VecDeque<Result<HqpOutputCommandReceipt, String>>>,
        poll_responses: RefCell<VecDeque<Option<HqpOutputOperation>>>,
        sleeps: RefCell<Vec<u32>>,
    }

    impl ScriptedTransport {
        fn new(
            post_responses: Vec<Result<HqpOutputCommandReceipt, String>>,
            poll_responses: Vec<Option<HqpOutputOperation>>,
        ) -> Self {
            Self {
                post_responses: RefCell::new(post_responses.into()),
                poll_responses: RefCell::new(poll_responses.into()),
                sleeps: RefCell::new(Vec::new()),
            }
        }
    }

    impl OutputTransport for ScriptedTransport {
        fn post_command(
            &self,
            _request: HqpOutputCommandRequest,
        ) -> Pin<Box<dyn Future<Output = Result<HqpOutputCommandReceipt, String>> + '_>> {
            let response = self
                .post_responses
                .borrow_mut()
                .pop_front()
                .expect("post_command called more times than scripted");
            Box::pin(async move { response })
        }

        fn fetch_operation(
            &self,
            _zone_id: String,
            _operation_id: String,
        ) -> Pin<Box<dyn Future<Output = Option<HqpOutputOperation>> + '_>> {
            let response =
                self.poll_responses.borrow_mut().pop_front().expect(
                    "fetch_operation called more times than scripted (unexpected extra poll)",
                );
            Box::pin(async move { response })
        }

        fn sleep(&self, ms: u32) -> Pin<Box<dyn Future<Output = ()> + '_>> {
            self.sleeps.borrow_mut().push(ms);
            Box::pin(async {})
        }
    }

    #[tokio::test]
    async fn a_nonterminal_operation_polls_with_backoff_until_a_later_terminal_response() {
        let admitted = op("op-1", "discover", HqpOutputPhase::Admitted, None, 1, None);
        let checking = op("op-1", "discover", HqpOutputPhase::Checking, None, 2, None);
        let complete = op(
            "op-1",
            "discover",
            HqpOutputPhase::Complete,
            Some(HqpOutputOutcome::Complete),
            3,
            None,
        );
        let transport = ScriptedTransport::new(
            vec![Ok(receipt(admitted, fixture_projection(1, 1, 1)))],
            vec![Some(checking), Some(complete)],
        );

        let observed = Rc::new(RefCell::new(Vec::new()));
        let observed_for_closure = observed.clone();
        let outcome = drive_output_command(
            &transport,
            empty_request(HqpOutputAction::Discover),
            || true,
            |_id| None,
            |_projection| {},
            move |operation: HqpOutputOperation| observed_for_closure.borrow_mut().push(operation),
        )
        .await;

        assert!(!outcome.superseded);
        assert!(outcome.error.is_none());
        assert_eq!(
            outcome.terminal.as_ref().map(|o| o.phase),
            Some(HqpOutputPhase::Complete)
        );
        assert_eq!(observed.borrow().len(), 3, "admission snapshot + 2 polls");
        assert_eq!(
            transport.sleeps.borrow().as_slice(),
            &[POLL_INITIAL_DELAY_MS, POLL_INITIAL_DELAY_MS * 2],
            "backoff must actually double between successive non-terminal polls"
        );
    }

    #[tokio::test]
    async fn a_fresh_aggregate_operation_beats_a_stale_nonterminal_poll_response() {
        // The poll response itself is still "connecting" (non-terminal), but the aggregate
        // projection (simulating an SSE-triggered background refresh landing mid-poll) already
        // shows this exact operation complete, with a later updated_at. The driver must stop on
        // the aggregate's terminal copy rather than keep polling because the raw poll disagreed.
        let admitted = op("op-2", "select", HqpOutputPhase::Admitted, None, 1, None);
        let stale_poll = op("op-2", "select", HqpOutputPhase::Connecting, None, 2, None);
        let fresh_aggregate = op(
            "op-2",
            "select",
            HqpOutputPhase::Complete,
            Some(HqpOutputOutcome::Complete),
            99,
            None,
        );
        let transport = ScriptedTransport::new(
            vec![Ok(receipt(admitted, fixture_projection(1, 1, 1)))],
            vec![Some(stale_poll)],
        );

        let outcome = drive_output_command(
            &transport,
            empty_request(HqpOutputAction::Select {
                route_id: "r1".to_string(),
            }),
            || true,
            move |id| (id == "op-2").then(|| fresh_aggregate.clone()),
            |_projection| {},
            |_operation| {},
        )
        .await;

        assert_eq!(outcome.terminal.as_ref().map(|o| o.updated_at), Some(99));
        assert_eq!(
            outcome.terminal.as_ref().map(|o| o.phase),
            Some(HqpOutputPhase::Complete),
            "the fresher aggregate copy must win over the stale raw poll response"
        );
    }

    #[tokio::test]
    async fn instance_change_or_stop_supersedes_a_held_operation_before_any_poll_fires() {
        let admitted = op("op-3", "select", HqpOutputPhase::Admitted, None, 1, None);
        let transport = ScriptedTransport::new(
            vec![Ok(receipt(admitted, fixture_projection(1, 1, 1)))],
            vec![], // must never be consumed: superseded before the first poll
        );
        let calls = Rc::new(RefCell::new(0u32));
        let calls_for_closure = calls.clone();
        // True on the first (post-admission) check only — simulates a newer mutation, Stop, or
        // an instance switch superseding this one right as it's about to start polling.
        let still_current = move || {
            let mut n = calls_for_closure.borrow_mut();
            *n += 1;
            *n <= 1
        };
        let observed = Rc::new(RefCell::new(Vec::new()));
        let observed_for_closure = observed.clone();

        let outcome = drive_output_command(
            &transport,
            empty_request(HqpOutputAction::Select {
                route_id: "r1".to_string(),
            }),
            still_current,
            |_id| None,
            |_projection| {},
            move |operation: HqpOutputOperation| observed_for_closure.borrow_mut().push(operation),
        )
        .await;

        assert!(outcome.superseded);
        assert!(outcome.terminal.is_none());
        assert_eq!(
            observed.borrow().len(),
            1,
            "the admission snapshot publishes once, but nothing further after supersession — \
             and no poll was consumed from the (empty) script"
        );
    }

    #[tokio::test]
    async fn a_setup_typed_result_arriving_only_on_a_later_poll_is_still_captured() {
        // The exact regression: the admission-time receipt has no typed result yet — only once
        // the operation actually completes does the backend populate `result` — so a caller that
        // assumed the initial receipt already carried it would see nothing.
        let admitted = op(
            "op-4",
            "setup_preview",
            HqpOutputPhase::Admitted,
            None,
            1,
            None,
        );
        let complete_with_result = op(
            "op-4",
            "setup_preview",
            HqpOutputPhase::Complete,
            Some(HqpOutputOutcome::Complete),
            2,
            Some(HqpOutputResult::SetupPreview(HqpSetupPreview {
                preserved_controls: 0,
                relay_option: None,
                preview_id: "prev-1".to_string(),
                applicable: true,
                blocker: None,
                current: vec![],
                changes: vec![],
                backup_sha256: "abc".to_string(),
                proposed_sha256: "def".to_string(),
            })),
        );
        let transport = ScriptedTransport::new(
            vec![Ok(receipt(admitted, fixture_projection(1, 1, 1)))],
            vec![Some(complete_with_result)],
        );

        let outcome = drive_output_command(
            &transport,
            empty_request(HqpOutputAction::SetupPreview),
            || true,
            |_id| None,
            |_projection| {},
            |_operation| {},
        )
        .await;

        match outcome.terminal.and_then(|operation| operation.result) {
            Some(HqpOutputResult::SetupPreview(preview)) => {
                assert_eq!(preview.preview_id, "prev-1");
            }
            other => panic!(
                "expected a typed SetupPreview result on the terminal operation, got {other:?}"
            ),
        }
    }

    #[tokio::test]
    async fn transient_poll_errors_retry_and_recover_before_giving_up() {
        let admitted = op("op-5", "discover", HqpOutputPhase::Admitted, None, 1, None);
        let complete = op(
            "op-5",
            "discover",
            HqpOutputPhase::Complete,
            Some(HqpOutputOutcome::Complete),
            5,
            None,
        );
        let transport = ScriptedTransport::new(
            vec![Ok(receipt(admitted, fixture_projection(1, 1, 1)))],
            vec![None, None, Some(complete)], // two transient failures, then success
        );

        let outcome = drive_output_command(
            &transport,
            empty_request(HqpOutputAction::Discover),
            || true,
            |_id| None,
            |_projection| {},
            |_operation| {},
        )
        .await;

        assert!(
            outcome.error.is_none(),
            "must recover after transient poll failures, not give up: {:?}",
            outcome.error
        );
        assert_eq!(
            outcome.terminal.as_ref().map(|o| o.phase),
            Some(HqpOutputPhase::Complete)
        );
    }

    #[tokio::test]
    async fn giving_up_after_too_many_consecutive_poll_errors_reports_a_recoverable_error() {
        let admitted = op("op-6", "discover", HqpOutputPhase::Admitted, None, 1, None);
        let transport = ScriptedTransport::new(
            vec![Ok(receipt(admitted, fixture_projection(1, 1, 1)))],
            vec![None; POLL_MAX_CONSECUTIVE_ERRORS as usize],
        );

        let outcome = drive_output_command(
            &transport,
            empty_request(HqpOutputAction::Discover),
            || true,
            |_id| None,
            |_projection| {},
            |_operation| {},
        )
        .await;

        assert!(outcome.terminal.is_none());
        assert!(
            outcome
                .error
                .as_deref()
                .is_some_and(|message| message.contains("Refresh")),
            "must surface a recoverable error pointing at the manual Refresh fallback, got {:?}",
            outcome.error
        );
    }

    #[tokio::test]
    async fn a_post_command_transport_error_is_reported_without_touching_operation_state() {
        let transport =
            ScriptedTransport::new(vec![Err("network unreachable".to_string())], vec![]);
        let outcome = drive_output_command(
            &transport,
            empty_request(HqpOutputAction::Discover),
            || true,
            |_id| None,
            |_projection| panic!("no projection to apply when the POST itself failed"),
            |_operation| panic!("no operation to track when the POST itself failed"),
        )
        .await;

        assert!(!outcome.superseded);
        assert_eq!(outcome.error.as_deref(), Some("network unreachable"));
        assert!(outcome.terminal.is_none());
    }

    /// **The exact regression**: a held operation whose polls are failing (e.g. the network is
    /// flaky right as Stop happens) must report `superseded: true`, not a normal error, once a
    /// newer mutation/Stop has superseded it — otherwise the caller (seeing `superseded: false`)
    /// would apply this stale error on top of whatever fresher busy/error state the newer
    /// mutation already established. Covers both give-up paths: too many consecutive poll
    /// errors, and exhausting all attempts.
    #[tokio::test]
    async fn a_held_operation_superseded_during_the_error_retry_stretch_reports_superseded_not_error(
    ) {
        let admitted = op("op-7", "select", HqpOutputPhase::Admitted, None, 1, None);
        let transport = ScriptedTransport::new(
            vec![Ok(receipt(admitted, fixture_projection(1, 1, 1)))],
            vec![None; POLL_MAX_CONSECUTIVE_ERRORS as usize],
        );
        // True through admission and every poll attempt except the very last consecutive-errors
        // check, where it flips false — simulating Stop superseding this call right as it's about
        // to give up.
        let calls = Rc::new(RefCell::new(0u32));
        let calls_for_closure = calls.clone();
        let still_current = move || {
            let mut n = calls_for_closure.borrow_mut();
            *n += 1;
            *n < (1 + POLL_MAX_CONSECUTIVE_ERRORS) // admission check + one check per poll attempt
        };

        let outcome = drive_output_command(
            &transport,
            empty_request(HqpOutputAction::Select {
                route_id: "r1".to_string(),
            }),
            still_current,
            |_id| None,
            |_projection| {},
            |_operation| {},
        )
        .await;

        assert!(
            outcome.superseded,
            "a supersession discovered exactly at the give-up point must be reported as \
             superseded, not as a normal recoverable error"
        );
    }

    #[tokio::test]
    async fn a_held_operation_superseded_exactly_when_poll_attempts_are_exhausted_reports_superseded(
    ) {
        let admitted = op("op-8", "select", HqpOutputPhase::Admitted, None, 1, None);
        // Every poll succeeds but stays non-terminal, so the loop runs out attempts naturally.
        let non_terminal = op("op-8", "select", HqpOutputPhase::Connecting, None, 2, None);
        let transport = ScriptedTransport::new(
            vec![Ok(receipt(admitted, fixture_projection(1, 1, 1)))],
            std::iter::repeat_with(|| Some(non_terminal.clone()))
                .take(POLL_MAX_ATTEMPTS as usize)
                .collect(),
        );
        let calls = Rc::new(RefCell::new(0u32));
        let calls_for_closure = calls.clone();
        // False only on the very last still_current check (immediately after the final poll
        // attempt, right before the function would otherwise return a timeout error).
        let total_checks = 1 + POLL_MAX_ATTEMPTS; // admission + one per attempt
        let still_current = move || {
            let mut n = calls_for_closure.borrow_mut();
            *n += 1;
            *n < total_checks
        };

        let outcome = drive_output_command(
            &transport,
            empty_request(HqpOutputAction::Select {
                route_id: "r1".to_string(),
            }),
            still_current,
            |_id| None,
            |_projection| {},
            |_operation| {},
        )
        .await;

        assert!(
            outcome.superseded,
            "supersession discovered exactly as attempts exhaust must be reported as \
             superseded, not as a normal timeout error"
        );
    }
}

/// Tests the exact `command_projection_should_apply` function — not a reimplemented formula.
/// `run_output_command`'s real `on_projection` closure (and its `still_current` check) calls this
/// function directly; these tests drive the real `drive_output_command` (transport-injectable,
/// no Dioxus `spawn()` needed to exercise it) with `still_current`/`on_projection` callbacks that
/// call `command_projection_should_apply` against **real** `Signal<OutputCommandFence>` state, in
/// the same shape `run_output_command` constructs them. A regression that changed the production
/// closure to pass the wrong fence into that call, or changed `command_projection_should_apply`'s
/// own body to read the wrong fence, would make these tests fail — unlike a test that only
/// reimplements the guard formula on plain values.
#[cfg(test)]
mod command_projection_guard_tests {
    use super::*;
    use std::future::Future;
    use std::pin::Pin;

    fn fixture_projection(
        source_epoch: u64,
        output_revision: u64,
        aggregate_revision: u64,
    ) -> HqpOutputProjection {
        HqpOutputProjection {
            zone_id: "hqplayer:living".to_string(),
            instance: "living".to_string(),
            source_epoch,
            aggregate_revision,
            output_revision,
            route_generation: 1,
            availability: crate::app::api::HqpOutputAvailabilityOrUnknown::Known(
                crate::app::api::HqpOutputAvailability::Available,
            ),
            relay: Default::default(),
            routes: vec![],
            selected_route_id: None,
            desired_destination: None,
            observed_forwarding_destination: None,
            session: None,
            discovery: None,
            dac_observations: vec![],
            native: Default::default(),
            current_operation_id: None,
            operations: vec![],
            last_error: None,
            observed_at: 0,
        }
    }

    fn terminal_op(operation_id: &str) -> HqpOutputOperation {
        HqpOutputOperation {
            operation_id: operation_id.to_string(),
            correlation_id: None,
            request_fingerprint: "fp".to_string(),
            zone_id: "hqplayer:living".to_string(),
            action: "select".to_string(),
            route_id: None,
            phase: HqpOutputPhase::Complete,
            outcome: Some(crate::app::api::HqpOutputOutcome::Complete),
            source_epoch: 1,
            output_revision_at_admission: 1,
            route_generation: None,
            admitted_at: 0,
            updated_at: 1,
            detail: None,
            result: None,
            evidence: crate::app::api::HqpOutputEvidence::default(),
        }
    }

    fn empty_request() -> HqpOutputCommandRequest {
        HqpOutputCommandRequest {
            zone_id: "hqplayer:living".to_string(),
            correlation_id: None,
            expected_source_epoch: None,
            expected_output_revision: None,
            action: HqpOutputAction::Select {
                route_id: "r1".to_string(),
            },
        }
    }

    /// One delayed POST, immediately terminal — enough to exercise the projection-apply guard at
    /// the point the response arrives, without needing the poll loop for this specific test.
    struct OneShotTransport {
        projection: HqpOutputProjection,
    }

    impl OutputTransport for OneShotTransport {
        fn post_command(
            &self,
            _request: HqpOutputCommandRequest,
        ) -> Pin<Box<dyn Future<Output = Result<HqpOutputCommandReceipt, String>> + '_>> {
            let receipt = HqpOutputCommandReceipt {
                accepted: true,
                operation: terminal_op("op-guard"),
                projection: self.projection.clone(),
            };
            Box::pin(async move { Ok(receipt) })
        }

        fn fetch_operation(
            &self,
            _zone_id: String,
            _operation_id: String,
        ) -> Pin<Box<dyn Future<Output = Option<HqpOutputOperation>> + '_>> {
            Box::pin(async { panic!("terminal on admission; no poll expected") })
        }

        fn sleep(&self, _ms: u32) -> Pin<Box<dyn Future<Output = ()> + '_>> {
            Box::pin(async { panic!("terminal on admission; no poll expected") })
        }
    }

    #[tokio::test]
    async fn a_command_response_resolving_after_an_unrelated_background_refresh_still_applies() {
        let dom = VirtualDom::new(|| rsx! { div {} });
        let _runtime_guard = dioxus::dioxus_core::RuntimeGuard::new(dom.runtime());
        let mut mutation_fence =
            dom.in_scope(ScopeId::ROOT, || Signal::new(OutputCommandFence::default()));
        let mutation_generation = mutation_fence.write().begin(); // command A issues

        // An unrelated background refresh (mirrors refresh_outputs) fires and bumps its own,
        // separate read_fence while A's POST is still "in flight" — mutation_fence is untouched.
        let mut read_fence =
            dom.in_scope(ScopeId::ROOT, || Signal::new(OutputCommandFence::default()));
        read_fence.write().begin();

        let outputs: Signal<Option<HqpOutputProjection>> =
            dom.in_scope(ScopeId::ROOT, || Signal::new(None));
        let loaded_once: Signal<bool> = dom.in_scope(ScopeId::ROOT, || Signal::new(false));
        let transport = OneShotTransport {
            projection: fixture_projection(3, 6, 105),
        };

        let outcome = drive_output_command(
            &transport,
            empty_request(),
            move || command_projection_should_apply(mutation_fence, mutation_generation),
            |_id| None,
            move |projection| {
                if command_projection_should_apply(mutation_fence, mutation_generation) {
                    apply_projection_read(outputs, loaded_once, Some(projection));
                }
            },
            |_operation| {},
        )
        .await;

        assert!(!outcome.superseded);
        assert_eq!(
            outputs.read().as_ref().map(|p| p.output_revision),
            Some(6),
            "command A's own projection must still be applied through the real \
             command_projection_should_apply call despite the unrelated background refresh \
             having bumped read_fence — if the production closure regressed to gate on \
             read_fence instead, this would be None"
        );
    }

    #[tokio::test]
    async fn a_command_response_resolving_after_a_genuinely_newer_mutation_is_rejected() {
        let dom = VirtualDom::new(|| rsx! { div {} });
        let _runtime_guard = dioxus::dioxus_core::RuntimeGuard::new(dom.runtime());
        // Reverse arrival order: a genuinely newer mutation B is issued (bumping mutation_fence
        // itself) before A's POST resolves. This is a real supersession, not an unrelated
        // background refresh, and command_projection_should_apply must reject A's projection.
        let mut mutation_fence =
            dom.in_scope(ScopeId::ROOT, || Signal::new(OutputCommandFence::default()));
        let mutation_generation_a = mutation_fence.write().begin();
        mutation_fence.write().begin(); // mutation B issued before A's POST resolves

        let outputs: Signal<Option<HqpOutputProjection>> =
            dom.in_scope(ScopeId::ROOT, || Signal::new(None));
        let loaded_once: Signal<bool> = dom.in_scope(ScopeId::ROOT, || Signal::new(false));
        let transport = OneShotTransport {
            projection: fixture_projection(3, 6, 105),
        };

        let outcome = drive_output_command(
            &transport,
            empty_request(),
            move || command_projection_should_apply(mutation_fence, mutation_generation_a),
            |_id| None,
            move |projection| {
                if command_projection_should_apply(mutation_fence, mutation_generation_a) {
                    apply_projection_read(outputs, loaded_once, Some(projection));
                }
            },
            |_operation| {},
        )
        .await;

        assert!(outcome.superseded, "A is genuinely superseded by B");
        assert!(
            outputs.read().is_none(),
            "A's projection must not be applied once a real newer mutation superseded it"
        );
    }

    #[test]
    fn a_stale_background_refresh_resolving_after_a_newer_command_is_still_rejected_by_data_freshness(
    ) {
        // The complementary case: an old background GET (older revision) resolves after a newer
        // command already applied fresher data. should_apply_projection rejects it by data
        // revision regardless of any fence — this proves the data layer alone is sufficient even
        // without relying on fence ordering.
        let older = fixture_projection(3, 5, 100);
        let newer = fixture_projection(3, 6, 105);
        assert!(should_apply_projection(None, &newer));
        assert!(
            !should_apply_projection(Some(&newer), &older),
            "a stale background read must never overwrite fresher data already displayed"
        );
    }
}
