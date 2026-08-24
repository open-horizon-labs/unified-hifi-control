//! LAN-only Kizz voice gateway.
//!
//! Kizz performs wake-word detection and VAD on-device, then streams one
//! bounded 16 kHz mono utterance. UHC streams it to speech recognition, then
//! hands the transcript to a persistent Codex App Server thread whose only
//! music capability is UHC's MCP server. Kizz owns the response character.

use crate::api::AppState;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::Response;
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use tokio::time::{timeout, Duration};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message as DgMessage};

const SAMPLE_RATE: u32 = 16_000;
const MAX_UTTERANCE_BYTES: usize = SAMPLE_RATE as usize * 2 * 14;
const MIN_UTTERANCE_BYTES: usize = SAMPLE_RATE as usize / 2;
const STT_CONNECT_TIMEOUT: Duration = Duration::from_secs(4);
const CODEX_START_TIMEOUT: Duration = Duration::from_secs(20);
const CODEX_TURN_TIMEOUT: Duration = Duration::from_secs(30);
const DEEPGRAM_AUDIO_CHUNK_BYTES: usize = 16_000 * 2 * 80 / 1000;
const ELEVENLABS_AUDIO_CHUNK_BYTES: usize = 16_000 * 2 * 100 / 1000;
const ELEVENLABS_MIN_AUDIO_BYTES: usize = 16_000 * 2 * 21 / 10;

static CODEX_AGENT: OnceLock<Mutex<Option<CodexVoiceAgent>>> = OnceLock::new();
static RUNTIME: OnceLock<VoiceRuntime> = OnceLock::new();
static NEXT_STT_TURN_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
struct VoiceTurnContext {
    zone_id: Option<String>,
    zone_name: Option<String>,
    now_playing: Option<VoiceNowPlayingContext>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct VoiceNowPlayingContext {
    title: String,
    artist: String,
    album: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SttProvider {
    Deepgram,
    Assemblyai,
    Elevenlabs,
}

impl SttProvider {
    fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "deepgram" => Some(Self::Deepgram),
            "assemblyai" | "assembly" => Some(Self::Assemblyai),
            "elevenlabs" | "eleven" => Some(Self::Elevenlabs),
            _ => None,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Deepgram => "deepgram",
            Self::Assemblyai => "assemblyai",
            Self::Elevenlabs => "elevenlabs",
        }
    }
}

fn providers_for(primary: SttProvider) -> [SttProvider; 3] {
    match primary {
        SttProvider::Deepgram => [
            SttProvider::Deepgram,
            SttProvider::Assemblyai,
            SttProvider::Elevenlabs,
        ],
        SttProvider::Assemblyai => [
            SttProvider::Assemblyai,
            SttProvider::Elevenlabs,
            SttProvider::Deepgram,
        ],
        SttProvider::Elevenlabs => [
            SttProvider::Elevenlabs,
            SttProvider::Assemblyai,
            SttProvider::Deepgram,
        ],
    }
}

fn stt_model_name(provider: SttProvider) -> String {
    match provider {
        SttProvider::Deepgram => "flux-general-en".to_string(),
        SttProvider::Assemblyai => "u3-rt-pro".to_string(),
        SttProvider::Elevenlabs => std::env::var("ELEVENLABS_STT_MODEL")
            .unwrap_or_else(|_| "scribe_v2_realtime".to_string()),
    }
}

fn all_active_providers_finished(
    has_pending_device_commit: bool,
    finished: usize,
    active: usize,
) -> bool {
    has_pending_device_commit && active > 0 && finished >= active
}

fn provider_event_is_current(active_turn_id: u64, event_turn_id: u64) -> bool {
    active_turn_id == event_turn_id
}

struct VoiceRuntime {
    provider: tokio::sync::RwLock<SttProvider>,
    reliability: Mutex<VoiceReliability>,
}

#[derive(Default, Serialize)]
struct VoiceReliability {
    attempts: u64,
    connected: u64,
    failed: u64,
    completed: u64,
    recent: VecDeque<VoiceTurnRecord>,
}

#[derive(Serialize)]
struct VoiceTurnRecord {
    timestamp: u64,
    turn_id: u64,
    provider: &'static str,
    outcome: &'static str,
    latency_ms: Option<u64>,
    detail: Option<String>,
}

#[derive(Deserialize)]
pub struct ProviderRequest {
    provider: String,
}

fn runtime() -> &'static VoiceRuntime {
    RUNTIME.get_or_init(|| VoiceRuntime {
        provider: tokio::sync::RwLock::new(
            std::env::var("KIZZ_STT_PROVIDER")
                .ok()
                .and_then(|v| SttProvider::parse(&v))
                .unwrap_or(SttProvider::Deepgram),
        ),
        reliability: Mutex::new(VoiceReliability::default()),
    })
}

async fn record_turn(
    turn_id: u64,
    provider: SttProvider,
    outcome: &'static str,
    latency_ms: Option<u64>,
    detail: Option<String>,
) {
    let state = runtime();
    let mut reliability = state.reliability.lock().await;
    match outcome {
        "connected" => {
            reliability.attempts += 1;
            reliability.connected += 1;
        }
        "connection_failed" => {
            reliability.attempts += 1;
            reliability.failed += 1;
        }
        "failed" => reliability.failed += 1,
        "completed" => reliability.completed += 1,
        _ => {}
    }
    reliability.recent.push_back(VoiceTurnRecord {
        timestamp: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        turn_id,
        provider: provider.name(),
        outcome,
        latency_ms,
        detail,
    });
    while reliability.recent.len() > 200 {
        reliability.recent.pop_front();
    }
}

pub async fn provider_get() -> axum::Json<Value> {
    let provider = *runtime().provider.read().await;
    axum::Json(json!({"provider": provider.name()}))
}

pub async fn provider_post(
    axum::Json(request): axum::Json<ProviderRequest>,
) -> Result<axum::Json<Value>, (StatusCode, axum::Json<Value>)> {
    let Some(provider) = SttProvider::parse(&request.provider) else {
        return Err((
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"error":"provider must be deepgram, assemblyai, or elevenlabs"})),
        ));
    };
    *runtime().provider.write().await = provider;
    tracing::info!(
        provider = provider.name(),
        "Kizz STT provider changed at runtime"
    );
    Ok(axum::Json(json!({"provider": provider.name()})))
}

pub async fn reliability_get() -> axum::Json<Value> {
    let reliability = runtime().reliability.lock().await;
    axum::Json(
        serde_json::to_value(&*reliability).unwrap_or_else(|_| json!({"error":"unavailable"})),
    )
}

#[derive(Debug)]
enum DeepgramEvent {
    PartialTranscript(String),
    EndOfTurn {
        transcript: String,
        confidence: Option<f64>,
    },
    Failed(String),
}

fn deepgram_closed_event(
    close_was_requested: bool,
    transcript: Option<String>,
    confidence: Option<f64>,
) -> DeepgramEvent {
    match (close_was_requested, transcript) {
        (true, Some(transcript)) if !transcript.trim().is_empty() => DeepgramEvent::EndOfTurn {
            transcript,
            confidence,
        },
        _ => DeepgramEvent::Failed("Deepgram closed the stream".to_string()),
    }
}

async fn emit_provider_event(events: &mpsc::Sender<DeepgramEvent>, event: DeepgramEvent) -> bool {
    if events.send(event).await.is_err() {
        tracing::debug!("Kizz STT provider event receiver dropped");
        false
    } else {
        true
    }
}

struct SttTurn {
    provider: SttProvider,
    input: mpsc::Sender<SttInput>,
}

enum SttInput {
    Audio(Vec<u8>),
    Close,
}

struct ProviderEvent {
    turn_id: u64,
    provider: SttProvider,
    event: DeepgramEvent,
    latency_ms: u64,
}

impl SttTurn {
    async fn send_audio(&self, pcm: Vec<u8>) -> Result<(), String> {
        self.input
            .send(SttInput::Audio(pcm))
            .await
            .map_err(|_| "speech recognition task stopped".to_string())
    }

    async fn close(&self) -> Result<(), String> {
        self.input
            .send(SttInput::Close)
            .await
            .map_err(|_| "speech recognition task stopped before finalization".to_string())
    }
}

pub async fn voice_upgrade(State(state): State<AppState>, upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(move |socket| run_session(socket, state))
}

/// Start the persistent audio agent after the HTTP listener is live, so the
/// first person who speaks to Kizz does not pay App Server startup latency.
pub async fn prewarm() {
    let agent = CODEX_AGENT.get_or_init(|| Mutex::new(None));
    let mut _conversation_guard = agent.lock().await;
    if _conversation_guard.is_some() {
        return;
    }
    match timeout(CODEX_START_TIMEOUT, CodexVoiceAgent::start()).await {
        Ok(Ok(started)) => *_conversation_guard = Some(started),
        Ok(Err(error)) => tracing::warn!(%error, "Kizz Codex agent prewarm failed"),
        Err(_) => tracing::warn!("Kizz Codex agent prewarm timed out"),
    }
}

async fn run_session(mut socket: WebSocket, state: AppState) {
    tracing::info!("Kizz voice session opened");
    let mut utterance = Vec::<u8>::new();
    let mut current_zone_id = None::<String>;
    let (provider_events_tx, mut provider_events_rx) = mpsc::channel(16);
    let mut stt_turns = Vec::<SttTurn>::new();
    let mut pending_fallback = None::<(Vec<u8>, Option<String>)>;
    let mut turn_completed = false;
    let mut active_turn_id = 0u64;
    let mut finished_stt = 0usize;
    loop {
        tokio::select! {
            incoming = socket.recv() => {
                let Some(incoming) = incoming else { break };
                let message = match incoming {
                    Ok(message) => message,
                    Err(error) => {
                        tracing::warn!(%error, "Kizz voice session received a WebSocket error");
                        break;
                    }
                };
                match message {
                    Message::Binary(pcm) => {
                        utterance.extend_from_slice(&pcm);
                        if utterance.len() > MAX_UTTERANCE_BYTES {
                            let excess = utterance.len() - MAX_UTTERANCE_BYTES;
                            utterance.drain(..excess);
                        }
                        for turn in &stt_turns {
                            if let Err(error) = turn.send_audio(pcm.to_vec()).await {
                                tracing::debug!(
                                    provider = turn.provider.name(),
                                    %error,
                                    "Kizz STT provider stopped accepting audio"
                                );
                            }
                        }
                    }
                    Message::Text(message) => match parse_client_event(&message) {
                        Some(ClientEvent::Start { zone_id }) => {
                            utterance.clear();
                            current_zone_id = zone_id;
                            turn_completed = false;
                            pending_fallback = None;
                            let selected_provider = *runtime().provider.read().await;
                            active_turn_id = NEXT_STT_TURN_ID.fetch_add(1, Ordering::Relaxed);
                            let turn_id = active_turn_id;
                            stt_turns.clear();
                            finished_stt = 0;
                            let providers = providers_for(selected_provider);
                            let connections = providers.into_iter().map(|provider| {
                                let events = provider_events_tx.clone();
                                async move {
                                    let started = Instant::now();
                                    let result = match timeout(
                                        STT_CONNECT_TIMEOUT,
                                        start_provider_turn(turn_id, provider, events),
                                    )
                                    .await
                                    {
                                        Ok(result) => result,
                                        Err(_) => Err(format!(
                                            "{} connection timed out after {} ms",
                                            provider.name(),
                                            STT_CONNECT_TIMEOUT.as_millis()
                                        )),
                                    };
                                    (provider, result, started.elapsed().as_millis() as u64)
                                }
                            });
                            for (provider, result, connect_latency_ms) in futures::future::join_all(connections).await {
                                match result {
                                    Ok(turn) => {
                                        record_turn(
                                            turn_id,
                                            provider,
                                            "connected",
                                            Some(connect_latency_ms),
                                            Some(stt_model_name(provider)),
                                        ).await;
                                        stt_turns.push(turn);
                                    }
                                    Err(error) => {
                                        record_turn(turn_id, provider, "connection_failed", Some(connect_latency_ms), Some(error.clone())).await;
                                        tracing::warn!(provider = provider.name(), %error, "streaming speech recognition unavailable");
                                    }
                                }
                            }
                            if stt_turns.is_empty()
                                && send_event(&mut socket, json!({"type":"state","state":"clarify","message":"I could not connect to speech recognition. Please try again."})).await.is_err()
                            {
                                break;
                            }
                            tracing::info!(zone_id = current_zone_id.as_deref().unwrap_or("unknown"),
                                providers = stt_turns.len(),
                                "Kizz voice turn received device context");
                        }
                        Some(ClientEvent::Commit) if turn_completed => {
                            tracing::info!("Ignored device fallback commit after Flux completed the turn");
                        }
                        Some(ClientEvent::Commit) if pending_fallback.is_some() => {
                            tracing::info!("Ignored duplicate device fallback commit while Flux finalizes");
                        }
                        Some(ClientEvent::Commit) => {
                            let committed = std::mem::take(&mut utterance);
                            let zone_id = current_zone_id.take();
                            tracing::info!(bytes = committed.len(), "Kizz fallback utterance committed");
                            if committed.len() < MIN_UTTERANCE_BYTES {
                                if send_event(&mut socket,
                                    json!({"type":"state","state":"clarify","message":"I did not hear enough yet."})).await.is_err() {
                                    break;
                                }
                                continue;
                            }
                            if send_event(&mut socket, json!({"type":"state","state":"thinking"})).await.is_err() {
                                break;
                            }
                            if !stt_turns.is_empty() {
                                pending_fallback = Some((committed, zone_id));
                                for turn in &stt_turns {
                                    if let Err(error) = turn.close().await {
                                        tracing::debug!(
                                            provider = turn.provider.name(),
                                            %error,
                                            "Kizz STT provider stopped before finalization"
                                        );
                                    }
                                }
                                tracing::info!("Streaming STT finalization requested by device fallback");
                                continue;
                            }
                            turn_completed = true;
                            tracing::warn!("No streaming speech recognizer was available for the Kizz turn");
                            if send_event(&mut socket, json!({"type":"state","state":"clarify","message":"Speech recognition is unavailable. Please try once more."})).await.is_err() {
                                break;
                            }
                        }
                        None => tracing::warn!(%message, "Ignored unknown Kizz voice event"),
                    },
                    Message::Ping(payload) => {
                        if socket.send(Message::Pong(payload)).await.is_err() { break; }
                    }
                    Message::Close(_) => {
                        tracing::info!("Kizz voice session closed by client");
                        break;
                    }
                    _ => {}
                }
            }
            event = provider_events_rx.recv() => {
                let Some(ProviderEvent { turn_id, provider, event, latency_ms }) = event else { continue };
                if !provider_event_is_current(active_turn_id, turn_id) {
                    tracing::info!(turn_id, active_turn_id, provider = provider.name(),
                        "Ignored a late STT result from an earlier Kizz turn");
                    continue;
                }
                match event {
                    DeepgramEvent::PartialTranscript(transcript) => {
                        tracing::info!(%transcript, provider = provider.name(),
                            latency_ms,
                            "Streaming STT produced its first partial transcript");
                    }
                    DeepgramEvent::EndOfTurn { transcript, confidence }
                        if provider == SttProvider::Deepgram && !turn_completed => {
                        finished_stt += 1;
                        // Flux is useful as a timing/reference signal, but its
                        // endpoint is too eager for Kizz's conversational audio.
                        // Keep the observation in the reliability ledger and
                        // let AssemblyAI or the device-side VAD commit the turn.
                        tracing::info!(%transcript, confidence, provider = provider.name(),
                            latency_ms,
                            "Ignored Deepgram Flux endpoint for Kizz; retaining timing telemetry");
                    }
                    DeepgramEvent::EndOfTurn { transcript, confidence } if !turn_completed => {
                        finished_stt += 1;
                        turn_completed = true;
                        for turn in &stt_turns {
                            if let Err(error) = turn.close().await {
                                tracing::debug!(
                                    provider = turn.provider.name(),
                                    %error,
                                    "Kizz STT provider stopped before competitor finalization"
                                );
                            }
                        }
                        let (committed, zone_id) = pending_fallback.take()
                            .unwrap_or_else(|| (std::mem::take(&mut utterance), current_zone_id.take()));
                        tracing::info!(%transcript, confidence, provider = provider.name(), bytes = committed.len(), latency_ms,
                            "Streaming STT ended Kizz voice turn");
                        if send_event(&mut socket, json!({"type":"endpoint","reason":format!("{}_end_of_turn", provider.name()),"confidence":confidence})).await.is_err() {
                            break;
                        }
                        // Surface the recognition result immediately. The device can
                        // show what it heard while the Codex/MCP turn is running.
                        if send_event(&mut socket,
                            json!({"type":"transcript","text":transcript})).await.is_err() {
                            break;
                        }
                        if send_event(&mut socket, json!({"type":"state","state":"thinking"})).await.is_err() {
                            break;
                        }
                        let context = current_voice_context(&state, zone_id.as_deref()).await;
                        tracing::info!(
                            zone_id = context.zone_id.as_deref().unwrap_or("unknown"),
                            zone_name = context.zone_name.as_deref().unwrap_or("unknown"),
                            title = context.now_playing.as_ref().map(|track| track.title.as_str()).unwrap_or(""),
                            artist = context.now_playing.as_ref().map(|track| track.artist.as_str()).unwrap_or(""),
                            album = context.now_playing.as_ref().map(|track| track.album.as_str()).unwrap_or(""),
                            "Kizz Codex turn received current playback context"
                        );
                        let result = match codex_transcript_request(&transcript, &context).await {
                            Ok(result) => result,
                            Err(error) => {
                                tracing::warn!(%error, "Kizz Codex voice turn failed");
                                json!({"type":"state","state":"clarify","message":"I lost that thought. Please try once more."})
                            }
                        };
                        if send_event(&mut socket, result).await.is_err() {
                            break;
                        }
                    }
                    DeepgramEvent::EndOfTurn { transcript, confidence } => {
                        finished_stt += 1;
                        tracing::info!(%transcript, confidence, provider = provider.name(), latency_ms,
                            "Streaming STT competitor completed after the winning transcript");
                    }
                    DeepgramEvent::Failed(error) => {
                        finished_stt += 1;
                        if all_active_providers_finished(
                            pending_fallback.is_some(),
                            finished_stt,
                            stt_turns.len(),
                        ) {
                            pending_fallback.take();
                            tracing::warn!(provider = provider.name(), %error, "Streaming STT finalization failed");
                            turn_completed = true;
                            if send_event(&mut socket, json!({"type":"state","state":"clarify","message":"Speech recognition is unavailable. Please try once more."})).await.is_err() {
                                break;
                            }
                        } else {
                            tracing::warn!(provider = provider.name(), %error, "Streaming STT stream failed; awaiting another provider or device fallback endpoint");
                        }
                    }
                }
            }
        }
    }
    tracing::info!("Kizz voice session ended");
}

#[derive(Debug, PartialEq)]
enum ClientEvent {
    Start { zone_id: Option<String> },
    Commit,
}

fn parse_client_event(message: &str) -> Option<ClientEvent> {
    let event: Value = serde_json::from_str(message).ok()?;
    match event.get("type")?.as_str()? {
        "start" => Some(ClientEvent::Start {
            zone_id: event
                .pointer("/context/zone_id")
                .and_then(Value::as_str)
                .filter(|zone_id| !zone_id.is_empty())
                .map(str::to_owned),
        }),
        "commit" => Some(ClientEvent::Commit),
        _ => None,
    }
}

async fn start_provider_turn(
    turn_id: u64,
    provider: SttProvider,
    events: mpsc::Sender<ProviderEvent>,
) -> Result<SttTurn, String> {
    let (inner_tx, mut inner_rx) = mpsc::channel(8);
    let forward = events.clone();
    let started = Instant::now();
    tokio::spawn(async move {
        while let Some(event) = inner_rx.recv().await {
            let latency_ms = started.elapsed().as_millis() as u64;
            match &event {
                DeepgramEvent::PartialTranscript(transcript) => {
                    record_turn(
                        turn_id,
                        provider,
                        "first_partial",
                        Some(latency_ms),
                        Some(transcript.clone()),
                    )
                    .await;
                }
                DeepgramEvent::EndOfTurn {
                    transcript,
                    confidence,
                } => {
                    let outcome = if provider == SttProvider::Deepgram {
                        "endpoint_hint"
                    } else {
                        "completed"
                    };
                    record_turn(
                        turn_id,
                        provider,
                        outcome,
                        Some(latency_ms),
                        Some(format!("{} (confidence={:?})", transcript, confidence)),
                    )
                    .await;
                }
                DeepgramEvent::Failed(error) => {
                    record_turn(
                        turn_id,
                        provider,
                        "failed",
                        Some(latency_ms),
                        Some(error.clone()),
                    )
                    .await;
                }
            }
            if forward
                .send(ProviderEvent {
                    turn_id,
                    provider,
                    event,
                    latency_ms,
                })
                .await
                .is_err()
            {
                break;
            }
        }
    });
    let mut turn = start_stt_turn(provider, inner_tx).await?;
    turn.provider = provider;
    Ok(turn)
}

async fn start_stt_turn(
    provider: SttProvider,
    events: mpsc::Sender<DeepgramEvent>,
) -> Result<SttTurn, String> {
    match provider {
        SttProvider::Assemblyai => start_assemblyai_turn(events).await,
        SttProvider::Deepgram => start_deepgram_turn(events).await,
        SttProvider::Elevenlabs => start_elevenlabs_turn(events).await,
    }
}

async fn start_deepgram_turn(events: mpsc::Sender<DeepgramEvent>) -> Result<SttTurn, String> {
    let key = std::env::var("DEEPGRAM_API_KEY")
        .map_err(|_| "DEEPGRAM_API_KEY is not configured".to_string())?;
    let eot_threshold =
        std::env::var("DEEPGRAM_EOT_THRESHOLD").unwrap_or_else(|_| "0.70".to_string());
    let eot_timeout_ms =
        std::env::var("DEEPGRAM_EOT_TIMEOUT_MS").unwrap_or_else(|_| "1800".to_string());
    let uri = format!(
        "wss://api.deepgram.com/v2/listen?model=flux-general-en&encoding=linear16&sample_rate=16000&eot_threshold={eot_threshold}&eot_timeout_ms={eot_timeout_ms}"
    );
    let mut request = uri
        .into_client_request()
        .map_err(|error| format!("invalid Deepgram URI: {error}"))?;
    request.headers_mut().insert(
        "Authorization",
        format!("Token {key}")
            .parse()
            .map_err(|error| format!("invalid Deepgram credential: {error}"))?,
    );
    let (socket, _) = timeout(
        Duration::from_secs(5),
        tokio_tungstenite::connect_async(request),
    )
    .await
    .map_err(|_| "Deepgram connection timed out".to_string())?
    .map_err(|error| format!("Deepgram connection failed: {error}"))?;
    let (mut output, mut input) = socket.split();
    let keyterms = std::env::var("DEEPGRAM_KEYTERMS")
        .unwrap_or_else(|_| "HiPhi,Kizz,Roon".to_string())
        .split(',')
        .map(str::trim)
        .filter(|term| !term.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if !keyterms.is_empty() {
        output
            .send(DgMessage::Text(
                json!({"type":"Configure","keyterms":keyterms}).to_string(),
            ))
            .await
            .map_err(|error| format!("Deepgram configuration failed: {error}"))?;
    }
    let (input_tx, mut input_rx) = mpsc::channel::<SttInput>(64);
    tokio::spawn(async move {
        let mut pending = Vec::<u8>::with_capacity(DEEPGRAM_AUDIO_CHUNK_BYTES * 2);
        let mut close_deadline = None::<tokio::time::Instant>;
        let mut input_open = true;
        let mut first_partial_sent = false;
        let mut latest_transcript = None::<String>;
        let mut latest_confidence = None::<f64>;
        loop {
            tokio::select! {
                command = input_rx.recv(), if input_open => {
                    match command {
                        Some(SttInput::Audio(audio)) => {
                            pending.extend_from_slice(&audio);
                            while pending.len() >= DEEPGRAM_AUDIO_CHUNK_BYTES {
                                let remainder = pending.split_off(DEEPGRAM_AUDIO_CHUNK_BYTES);
                                let chunk = std::mem::replace(&mut pending, remainder);
                                if let Err(error) = output.send(DgMessage::Binary(chunk)).await {
                                    if !emit_provider_event(
                                        &events,
                                        DeepgramEvent::Failed(error.to_string()),
                                    )
                                    .await
                                    {
                                        return;
                                    }
                                    return;
                                }
                            }
                        }
                        Some(SttInput::Close) | None => {
                            input_open = false;
                            if !pending.is_empty() {
                                if let Err(error) = output.send(DgMessage::Binary(
                                    std::mem::take(&mut pending))).await {
                                    if !emit_provider_event(
                                        &events,
                                        DeepgramEvent::Failed(error.to_string()),
                                    )
                                    .await
                                    {
                                        return;
                                    }
                                    return;
                                }
                            }
                            if let Err(error) = output.send(DgMessage::Text(
                                json!({"type":"CloseStream"}).to_string())).await {
                                if !emit_provider_event(
                                    &events,
                                    DeepgramEvent::Failed(error.to_string()),
                                )
                                .await
                                {
                                    return;
                                }
                                return;
                            }
                            close_deadline = Some(tokio::time::Instant::now() + Duration::from_secs(3));
                        }
                    }
                }
                message = input.next() => {
                    let Some(message) = message else {
                        if !emit_provider_event(
                            &events,
                            deepgram_closed_event(
                                !input_open,
                                latest_transcript.take(),
                                latest_confidence,
                            ),
                        )
                        .await
                        {
                            return;
                        }
                        return;
                    };
                    match message {
                        Ok(DgMessage::Text(text)) => {
                            let Ok(event) = serde_json::from_str::<Value>(&text) else { continue };
                            if event.get("type").and_then(Value::as_str) == Some("Error") {
                                if !emit_provider_event(
                                    &events,
                                    DeepgramEvent::Failed(event.to_string()),
                                )
                                .await
                                {
                                    return;
                                }
                                return;
                            }
                            if event.get("type").and_then(Value::as_str) == Some("TurnInfo") {
                                let transcript = event.get("transcript")
                                    .and_then(Value::as_str).unwrap_or("").trim();
                                if !transcript.is_empty() {
                                    latest_transcript = Some(transcript.to_string());
                                }
                                latest_confidence = event.get("end_of_turn_confidence")
                                    .and_then(Value::as_f64).or(latest_confidence);
                            }
                            if event.get("type").and_then(Value::as_str) == Some("TurnInfo") &&
                                event.get("event").and_then(Value::as_str) == Some("EndOfTurn") {
                                let transcript = event.get("transcript")
                                    .and_then(Value::as_str).unwrap_or("").trim().to_string();
                                let confidence = event.get("end_of_turn_confidence")
                                    .and_then(Value::as_f64);
                                if transcript.is_empty() {
                                    if !emit_provider_event(
                                        &events,
                                        DeepgramEvent::Failed(
                                            "Deepgram ended an empty turn".to_string(),
                                        ),
                                    )
                                    .await
                                    {
                                        return;
                                    }
                                } else if !emit_provider_event(
                                    &events,
                                    DeepgramEvent::EndOfTurn {
                                        transcript,
                                        confidence,
                                    },
                                )
                                .await
                                {
                                    return;
                                }
                                return;
                            }
                            if !first_partial_sent &&
                                event.get("type").and_then(Value::as_str) == Some("TurnInfo") {
                                let transcript = event.get("transcript")
                                    .and_then(Value::as_str).unwrap_or("").trim();
                                if !transcript.is_empty() {
                                    first_partial_sent = true;
                                    if !emit_provider_event(
                                        &events,
                                        DeepgramEvent::PartialTranscript(transcript.to_string()),
                                    )
                                    .await
                                    {
                                        return;
                                    }
                                }
                            }
                        }
                        Ok(DgMessage::Close(_)) => {
                            if !emit_provider_event(
                                &events,
                                deepgram_closed_event(
                                    !input_open,
                                    latest_transcript.take(),
                                    latest_confidence,
                                ),
                            )
                            .await
                            {
                                return;
                            }
                            return;
                        }
                        Err(error) => {
                            if !emit_provider_event(
                                &events,
                                DeepgramEvent::Failed(error.to_string()),
                            )
                            .await
                            {
                                return;
                            }
                            return;
                        }
                        _ => {}
                    }
                }
                _ = tokio::time::sleep_until(close_deadline.unwrap_or_else(||
                    tokio::time::Instant::now() + Duration::from_secs(86_400))),
                    if close_deadline.is_some() => {
                    if !emit_provider_event(
                        &events,
                        DeepgramEvent::Failed("Deepgram finalization timed out".to_string()),
                    )
                    .await
                    {
                        return;
                    }
                    return;
                }
            }
        }
    });
    Ok(SttTurn {
        provider: SttProvider::Deepgram,
        input: input_tx,
    })
}

fn elevenlabs_audio_message(pcm: &[u8], commit: bool) -> String {
    json!({
        "message_type": "input_audio_chunk",
        "audio_base_64": BASE64_STANDARD.encode(pcm),
        "commit": commit,
        "sample_rate": SAMPLE_RATE,
    })
    .to_string()
}

fn elevenlabs_padding_bytes(audio_bytes_sent: usize) -> usize {
    ELEVENLABS_MIN_AUDIO_BYTES.saturating_sub(audio_bytes_sent)
}

fn parse_elevenlabs_event(text: &str) -> Option<DeepgramEvent> {
    let event = serde_json::from_str::<Value>(text).ok()?;
    let message_type = event.get("message_type").and_then(Value::as_str)?;
    match message_type {
        "partial_transcript" => event
            .get("text")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|transcript| !transcript.is_empty())
            .map(|transcript| DeepgramEvent::PartialTranscript(transcript.to_string())),
        "committed_transcript" => {
            let transcript = event
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            if transcript.is_empty() {
                Some(DeepgramEvent::Failed(
                    "ElevenLabs committed an empty transcript".to_string(),
                ))
            } else {
                Some(DeepgramEvent::EndOfTurn {
                    transcript,
                    confidence: None,
                })
            }
        }
        "auth_error"
        | "quota_exceeded"
        | "transcriber_error"
        | "input_error"
        | "invalid_request"
        | "error"
        | "commit_throttled"
        | "unaccepted_terms"
        | "rate_limited"
        | "queue_overflow"
        | "resource_exhausted"
        | "session_time_limit_exceeded"
        | "chunk_size_exceeded"
        | "insufficient_audio_activity" => Some(DeepgramEvent::Failed(format!(
            "ElevenLabs {message_type}: {}",
            event.get("error").and_then(Value::as_str).unwrap_or(text)
        ))),
        _ => None,
    }
}

async fn start_elevenlabs_turn(events: mpsc::Sender<DeepgramEvent>) -> Result<SttTurn, String> {
    let key = std::env::var("ELEVENLABS_API_KEY")
        .or_else(|_| std::env::var("ELEVEN_LABS_API_KEY"))
        .map_err(|_| {
            "ELEVENLABS_API_KEY (or legacy ELEVEN_LABS_API_KEY) is not configured".to_string()
        })?;
    let model = stt_model_name(SttProvider::Elevenlabs);
    let endpoint = std::env::var("ELEVENLABS_STT_URL")
        .unwrap_or_else(|_| "wss://api.elevenlabs.io/v1/speech-to-text/realtime".to_string());
    let uri = format!(
        "{endpoint}?model_id={}&audio_format=pcm_16000&language_code=en&commit_strategy=manual",
        urlencoding::encode(&model)
    );
    let mut request = uri
        .into_client_request()
        .map_err(|error| format!("invalid ElevenLabs URI: {error}"))?;
    request.headers_mut().insert(
        "xi-api-key",
        key.parse()
            .map_err(|error| format!("invalid ElevenLabs credential: {error}"))?,
    );
    let (socket, _) = timeout(
        Duration::from_secs(5),
        tokio_tungstenite::connect_async(request),
    )
    .await
    .map_err(|_| "ElevenLabs connection timed out".to_string())?
    .map_err(|error| format!("ElevenLabs connection failed: {error}"))?;
    let (mut output, mut input) = socket.split();
    let (input_tx, mut input_rx) = mpsc::channel::<SttInput>(64);
    tokio::spawn(async move {
        let mut pending_audio = Vec::<u8>::with_capacity(ELEVENLABS_AUDIO_CHUNK_BYTES * 2);
        let mut first_partial_sent = false;
        let mut audio_bytes_sent = 0usize;
        let mut input_open = true;
        let mut close_deadline = None::<tokio::time::Instant>;
        loop {
            tokio::select! {
                command = input_rx.recv(), if input_open => {
                    match command {
                        Some(SttInput::Audio(audio)) => {
                            pending_audio.extend_from_slice(&audio);
                            while pending_audio.len() >= ELEVENLABS_AUDIO_CHUNK_BYTES {
                                let remainder = pending_audio.split_off(ELEVENLABS_AUDIO_CHUNK_BYTES);
                                let chunk = std::mem::replace(&mut pending_audio, remainder);
                                audio_bytes_sent += chunk.len();
                                if let Err(error) = output.send(DgMessage::Text(
                                    elevenlabs_audio_message(&chunk, false))).await {
                                    if !emit_provider_event(
                                        &events,
                                        DeepgramEvent::Failed(error.to_string()),
                                    )
                                    .await
                                    {
                                        return;
                                    }
                                    return;
                                }
                            }
                        }
                        Some(SttInput::Close) | None => {
                            input_open = false;
                            if !pending_audio.is_empty() {
                                audio_bytes_sent += pending_audio.len();
                                if let Err(error) = output.send(DgMessage::Text(
                                    elevenlabs_audio_message(&std::mem::take(&mut pending_audio), false))).await {
                                    if !emit_provider_event(
                                        &events,
                                        DeepgramEvent::Failed(error.to_string()),
                                    )
                                    .await
                                    {
                                        return;
                                    }
                                    return;
                                }
                            }
                            let mut padding_bytes = elevenlabs_padding_bytes(audio_bytes_sent);
                            while padding_bytes > 0 {
                                let chunk_bytes = padding_bytes.min(ELEVENLABS_AUDIO_CHUNK_BYTES);
                                if let Err(error) = output.send(DgMessage::Text(
                                    elevenlabs_audio_message(&vec![0; chunk_bytes], false))).await {
                                    if !emit_provider_event(
                                        &events,
                                        DeepgramEvent::Failed(error.to_string()),
                                    )
                                    .await
                                    {
                                        return;
                                    }
                                    return;
                                }
                                padding_bytes -= chunk_bytes;
                            }
                            // This matches the official SDK's manual commit wire message.
                            if let Err(error) = output.send(DgMessage::Text(
                                elevenlabs_audio_message(&[], true))).await {
                                if !emit_provider_event(
                                    &events,
                                    DeepgramEvent::Failed(error.to_string()),
                                )
                                .await
                                {
                                    return;
                                }
                                return;
                            }
                            close_deadline = Some(tokio::time::Instant::now() + Duration::from_secs(5));
                        }
                    }
                }
                message = input.next() => {
                    let Some(message) = message else {
                        if !emit_provider_event(
                            &events,
                            DeepgramEvent::Failed("ElevenLabs closed the stream".to_string()),
                        )
                        .await
                        {
                            return;
                        }
                        return;
                    };
                    match message {
                        Ok(DgMessage::Text(text)) => {
                            if let Some(event) = parse_elevenlabs_event(&text) {
                                match event {
                                    DeepgramEvent::PartialTranscript(_) if first_partial_sent => {}
                                    DeepgramEvent::PartialTranscript(_) => {
                                        first_partial_sent = true;
                                        if !emit_provider_event(&events, event).await {
                                            return;
                                        }
                                    }
                                    _ => {
                                        if !emit_provider_event(&events, event).await {
                                            return;
                                        }
                                        return;
                                    }
                                }
                            }
                        }
                        Ok(DgMessage::Close(_)) => {
                            if !emit_provider_event(
                                &events,
                                DeepgramEvent::Failed("ElevenLabs closed the stream".to_string()),
                            )
                            .await
                            {
                                return;
                            }
                            return;
                        }
                        Err(error) => {
                            if !emit_provider_event(
                                &events,
                                DeepgramEvent::Failed(error.to_string()),
                            )
                            .await
                            {
                                return;
                            }
                            return;
                        }
                        _ => {}
                    }
                }
                _ = tokio::time::sleep_until(close_deadline.unwrap_or_else(||
                    tokio::time::Instant::now() + Duration::from_secs(86_400))),
                    if close_deadline.is_some() => {
                    if !emit_provider_event(
                        &events,
                        DeepgramEvent::Failed("ElevenLabs finalization timed out".to_string()),
                    )
                    .await
                    {
                        return;
                    }
                    return;
                }
            }
        }
    });
    Ok(SttTurn {
        provider: SttProvider::Elevenlabs,
        input: input_tx,
    })
}

async fn start_assemblyai_turn(events: mpsc::Sender<DeepgramEvent>) -> Result<SttTurn, String> {
    let key = std::env::var("ASSEMBLYAI_API_KEY")
        .map_err(|_| "ASSEMBLYAI_API_KEY is not configured".to_string())?;
    let min_silence =
        std::env::var("ASSEMBLYAI_MIN_TURN_SILENCE_MS").unwrap_or_else(|_| "300".to_string());
    let max_silence =
        std::env::var("ASSEMBLYAI_MAX_TURN_SILENCE_MS").unwrap_or_else(|_| "1200".to_string());
    let uri = format!(
        "wss://streaming.assemblyai.com/v3/ws?sample_rate=16000&speech_model=u3-rt-pro&format_turns=false&min_turn_silence={min_silence}&max_turn_silence={max_silence}"
    );
    let mut request = uri
        .into_client_request()
        .map_err(|error| format!("invalid AssemblyAI URI: {error}"))?;
    request.headers_mut().insert(
        "Authorization",
        key.parse()
            .map_err(|error| format!("invalid AssemblyAI credential: {error}"))?,
    );
    let (socket, _) = timeout(
        Duration::from_secs(5),
        tokio_tungstenite::connect_async(request),
    )
    .await
    .map_err(|_| "AssemblyAI connection timed out".to_string())?
    .map_err(|error| format!("AssemblyAI connection failed: {error}"))?;
    let (mut output, mut input) = socket.split();
    let (input_tx, mut input_rx) = mpsc::channel::<SttInput>(64);
    tokio::spawn(async move {
        // The device emits 32 ms (512-sample) frames. AssemblyAI requires
        // audio messages to be at least 50 ms, so coalesce frames before
        // forwarding them and pad only the final tail when necessary.
        const MIN_AUDIO_BYTES: usize = 16_000 * 2 * 50 / 1000;
        const AUDIO_CHUNK_BYTES: usize = 16_000 * 2 * 80 / 1000;
        let mut pending_audio = Vec::<u8>::with_capacity(AUDIO_CHUNK_BYTES * 2);
        let mut first_partial_sent = false;
        loop {
            tokio::select! {
                command = input_rx.recv() => {
                    match command {
                        Some(SttInput::Audio(audio)) => {
                            pending_audio.extend_from_slice(&audio);
                            while pending_audio.len() >= AUDIO_CHUNK_BYTES {
                                let remainder = pending_audio.split_off(AUDIO_CHUNK_BYTES);
                                let chunk = std::mem::replace(&mut pending_audio, remainder);
                                if let Err(error) = output.send(DgMessage::Binary(chunk)).await {
                                    if !emit_provider_event(
                                        &events,
                                        DeepgramEvent::Failed(error.to_string()),
                                    )
                                    .await
                                    {
                                        return;
                                    }
                                    return;
                                }
                            }
                        }
                        Some(SttInput::Close) | None => {
                            // Kizz's device-side VAD has already decided that this
                            // utterance is complete. Send the final buffered audio
                            // before ForceEndpoint; flushing first can make
                            // AssemblyAI finalize an empty/incomplete turn.
                            if !pending_audio.is_empty() {
                                if pending_audio.len() < MIN_AUDIO_BYTES {
                                    pending_audio.resize(MIN_AUDIO_BYTES, 0);
                                }
                                if let Err(error) = output.send(DgMessage::Binary(
                                    std::mem::take(&mut pending_audio))).await {
                                    if !emit_provider_event(
                                        &events,
                                        DeepgramEvent::Failed(error.to_string()),
                                    )
                                    .await
                                    {
                                        return;
                                    }
                                    return;
                                }
                            }
                            // ForceEndpoint flushes the final Turn; Terminate only
                            // closes the session and can discard an in-flight turn.
                            if let Err(error) = output.send(DgMessage::Text(
                                json!({"type":"ForceEndpoint"}).to_string())).await {
                                if !emit_provider_event(
                                    &events,
                                    DeepgramEvent::Failed(error.to_string()),
                                )
                                .await
                                {
                                    return;
                                }
                                return;
                            }
                            match tokio::time::timeout(Duration::from_secs(5), async {
                                while let Some(message) = input.next().await {
                                    match message {
                                        Ok(DgMessage::Text(text)) => {
                                            let Ok(event) = serde_json::from_str::<Value>(&text) else { continue };
                                            if event.get("type").and_then(Value::as_str) == Some("Turn") &&
                                                event.get("end_of_turn").and_then(Value::as_bool) == Some(true) {
                                                let transcript = event.get("transcript")
                                                    .and_then(Value::as_str).unwrap_or("").trim().to_string();
                                                if transcript.is_empty() {
                                                    return Err("AssemblyAI ended an empty turn".to_string());
                                                }
                                                let confidence = event.get("end_of_turn_confidence").and_then(Value::as_f64);
                                                if !emit_provider_event(
                                                    &events,
                                                    DeepgramEvent::EndOfTurn { transcript, confidence },
                                                )
                                                .await
                                                {
                                                    return Err("Kizz provider event receiver dropped".to_string());
                                                }
                                                return Ok(());
                                            }
                                            if event.get("type").and_then(Value::as_str) == Some("Error") {
                                                return Err(event.to_string());
                                            }
                                        }
                                        Ok(DgMessage::Close(_)) => return Err("AssemblyAI closed the stream".to_string()),
                                        Err(error) => return Err(error.to_string()),
                                        _ => {}
                                    }
                                }
                                Err("AssemblyAI closed the stream".to_string())
                            }).await {
                                Ok(Ok(())) => {}
                                Ok(Err(error)) => {
                                    if !emit_provider_event(&events, DeepgramEvent::Failed(error)).await {
                                        return;
                                    }
                                }
                                Err(_) => {
                                    if !emit_provider_event(
                                        &events,
                                        DeepgramEvent::Failed(
                                            "AssemblyAI finalization timed out".to_string(),
                                        ),
                                    )
                                    .await
                                    {
                                        return;
                                    }
                                }
                            }
                            return;
                        }
                    }
                }
                message = input.next() => {
                    let Some(message) = message else {
                        if !emit_provider_event(
                            &events,
                            DeepgramEvent::Failed("AssemblyAI closed the stream".to_string()),
                        )
                        .await
                        {
                            return;
                        }
                        return;
                    };
                    match message {
                        Ok(DgMessage::Text(text)) => {
                            let Ok(event) = serde_json::from_str::<Value>(&text) else { continue };
                            if event.get("type").and_then(Value::as_str) == Some("Error") {
                                if !emit_provider_event(
                                    &events,
                                    DeepgramEvent::Failed(event.to_string()),
                                )
                                .await
                                {
                                    return;
                                }
                                return;
                            }
                            if event.get("type").and_then(Value::as_str) == Some("Turn") &&
                                event.get("end_of_turn").and_then(Value::as_bool) == Some(true) {
                                let transcript = event.get("transcript").and_then(Value::as_str).unwrap_or("").trim().to_string();
                                if transcript.is_empty() {
                                    if !emit_provider_event(
                                        &events,
                                        DeepgramEvent::Failed(
                                            "AssemblyAI ended an empty turn".to_string(),
                                        ),
                                    )
                                    .await
                                    {
                                        return;
                                    }
                                } else {
                                    let confidence = event.get("end_of_turn_confidence").and_then(Value::as_f64);
                                    if !emit_provider_event(
                                        &events,
                                        DeepgramEvent::EndOfTurn { transcript, confidence },
                                    )
                                    .await
                                    {
                                        return;
                                    }
                                }
                                return;
                            }
                            if !first_partial_sent &&
                                event.get("type").and_then(Value::as_str) == Some("Turn") {
                                let transcript = event.get("transcript")
                                    .and_then(Value::as_str).unwrap_or("").trim();
                                if !transcript.is_empty() {
                                    first_partial_sent = true;
                                    if !emit_provider_event(
                                        &events,
                                        DeepgramEvent::PartialTranscript(transcript.to_string()),
                                    )
                                    .await
                                    {
                                        return;
                                    }
                                }
                            }
                        }
                        Ok(DgMessage::Close(_)) => {
                            if !emit_provider_event(
                                &events,
                                DeepgramEvent::Failed("AssemblyAI closed the stream".to_string()),
                            )
                            .await
                            {
                                return;
                            }
                            return;
                        }
                        Err(error) => {
                            if !emit_provider_event(
                                &events,
                                DeepgramEvent::Failed(error.to_string()),
                            )
                            .await
                            {
                                return;
                            }
                            return;
                        }
                        _ => {}
                    }
                }
            }
        }
    });
    Ok(SttTurn {
        provider: SttProvider::Assemblyai,
        input: input_tx,
    })
}

async fn codex_transcript_request(
    transcript: &str,
    context: &VoiceTurnContext,
) -> Result<Value, String> {
    let agent = CODEX_AGENT.get_or_init(|| Mutex::new(None));
    let mut _conversation_guard = agent.lock().await;
    if _conversation_guard.is_none() {
        *_conversation_guard = Some(
            timeout(CODEX_START_TIMEOUT, CodexVoiceAgent::start())
                .await
                .map_err(|_| "Codex App Server startup timed out".to_string())??,
        );
    }

    let first = _conversation_guard
        .as_mut()
        .ok_or("Codex voice agent was not initialized")?
        .turn(transcript, context);
    let first = timeout(CODEX_TURN_TIMEOUT, first)
        .await
        .map_err(|_| "Codex voice turn timed out".to_string())?;
    if first.is_ok() {
        return first;
    }

    // App Server is local and disposable. Restart once after a broken pipe or
    // malformed terminal event, while preserving no false success state.
    if let Some(mut stale) = _conversation_guard.take() {
        let _ = stale.child.kill().await;
    }
    let mut restarted = timeout(CODEX_START_TIMEOUT, CodexVoiceAgent::start())
        .await
        .map_err(|_| "Codex App Server restart timed out".to_string())??;
    let result = timeout(CODEX_TURN_TIMEOUT, restarted.turn(transcript, context))
        .await
        .map_err(|_| "Codex voice turn timed out after restart".to_string())?;
    *_conversation_guard = Some(restarted);
    result
}

async fn current_voice_context(
    state: &AppState,
    current_zone_id: Option<&str>,
) -> VoiceTurnContext {
    let Some(zone_id) = current_zone_id else {
        return VoiceTurnContext::default();
    };
    let Some(zone) = state.aggregator.get_zone(zone_id).await else {
        return VoiceTurnContext {
            zone_id: Some(zone_id.to_string()),
            ..VoiceTurnContext::default()
        };
    };
    VoiceTurnContext {
        zone_id: Some(zone.zone_id),
        zone_name: Some(zone.zone_name),
        now_playing: zone.now_playing.map(|track| VoiceNowPlayingContext {
            title: track.title,
            artist: track.artist,
            album: track.album,
        }),
    }
}

fn build_codex_input(transcript: &str, context: &VoiceTurnContext) -> Result<String, String> {
    let context = serde_json::to_string(context)
        .map_err(|error| format!("Kizz device context could not be serialized: {error}"))?;
    Ok(format!(
        "Local speech recognition heard: {transcript}\nCurrent device context: {context}"
    ))
}

struct CodexVoiceAgent {
    child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    thread_id: String,
    next_id: u64,
}

impl CodexVoiceAgent {
    async fn start() -> Result<Self, String> {
        let port = std::env::var("UHC_PORT").unwrap_or_else(|_| "8088".to_string());
        let mcp_override = format!(
            "mcp_servers.unified-hifi-control={{url=\"http://127.0.0.1:{port}/mcp\",enabled=true}}"
        );
        let mut child = Command::new("codex")
            .args([
                "app-server",
                "--stdio",
                "-c",
                "mcp_servers.amazing-marvin.enabled=false",
                "-c",
                "mcp_servers.node_repl.enabled=false",
                "-c",
                "mcp_servers.home-assistant.enabled=false",
                "-c",
                &mcp_override,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| format!("Codex App Server is unavailable: {error}"))?;
        let stdin = child.stdin.take().ok_or("Codex App Server has no stdin")?;
        let stdout = child
            .stdout
            .take()
            .ok_or("Codex App Server has no stdout")?;
        let mut agent = Self {
            child,
            stdin,
            lines: BufReader::new(stdout).lines(),
            thread_id: String::new(),
            next_id: 1,
        };

        let initialize_id = agent
            .request(json!({
                "method": "initialize",
                "params": {
                    "clientInfo": {"name":"kizz-voice","version":"0.1"},
                    "capabilities": {"experimentalApi":true}
                }
            }))
            .await?;
        agent.wait_for_response(initialize_id).await?;
        agent
            .notify(json!({"method":"initialized","params":{}}))
            .await?;

        let model =
            std::env::var("KIZZ_CODEX_MODEL").unwrap_or_else(|_| "gpt-5.6-luna".to_string());
        let working_directory = std::env::temp_dir().join("kizz-codex-agent");
        tokio::fs::create_dir_all(&working_directory)
            .await
            .map_err(|error| format!("Kizz agent workspace is unavailable: {error}"))?;
        let thread_request_id = agent
            .request(json!({
                "method":"thread/start",
                "params": {
                    "ephemeral": true,
                    "cwd": working_directory,
                    "model": model,
                    "approvalPolicy": "on-request",
                    "sandbox": "read-only",
                    "baseInstructions": KIZZ_AGENT_INSTRUCTIONS
                }
            }))
            .await?;
        let response = agent.wait_for_response(thread_request_id).await?;
        agent.thread_id = response["result"]["thread"]["id"]
            .as_str()
            .ok_or("Codex App Server did not return a thread id")?
            .to_string();
        tracing::info!(
            thread_id = %agent.thread_id,
            "Kizz Codex App Server agent ready with UHC MCP"
        );
        Ok(agent)
    }

    async fn turn(
        &mut self,
        transcript: &str,
        context: &VoiceTurnContext,
    ) -> Result<Value, String> {
        let request_id = self
            .request(json!({
                "method":"turn/start",
                "params": {
                    "threadId": self.thread_id,
                    "input": [{
                        "type":"text",
                        "text": build_codex_input(transcript, context)?
                    }],
                    "effort": "low",
                    "outputSchema": kizz_result_schema()
                }
            }))
            .await?;
        self.wait_for_response(request_id).await?;

        while let Some(line) = self
            .lines
            .next_line()
            .await
            .map_err(|error| format!("Codex App Server stream failed: {error}"))?
        {
            let message: Value = match serde_json::from_str(&line) {
                Ok(message) => message,
                Err(_) => continue,
            };
            if message.get("id").is_some() && message.get("method").is_some() {
                tracing::info!(request = %message, "Kizz Codex App Server requested client input");
                if message["method"] == "mcpServer/elicitation/request"
                    && message["params"]["serverName"] == "unified-hifi-control"
                    && message["params"]["_meta"]["codex_approval_kind"] == "mcp_tool_call"
                {
                    tracing::info!(
                        tool = %message["params"]["_meta"]["tool_name"],
                        arguments = %message["params"]["_meta"]["tool_params"],
                        "Approved Kizz LAN UHC MCP call for this session"
                    );
                    let response = json!({
                        "id": message["id"],
                        "result": {
                            "action": "accept",
                            "content": {},
                            "_meta": { "persist": "session" }
                        }
                    });
                    self.write(response).await?;
                    continue;
                }
            }
            if message["method"] == "item/completed"
                && message["params"]["item"]["type"] == "mcpToolCall"
            {
                let item = &message["params"]["item"];
                tracing::info!(
                    server = %item["server"].as_str().unwrap_or(""),
                    tool = %item["tool"].as_str().unwrap_or(""),
                    arguments = %item["arguments"],
                    status = %item["status"],
                    error = %item["error"],
                    result = %item["result"],
                    "Kizz Codex MCP call completed"
                );
            }
            if message["method"] != "turn/completed"
                || message["params"]["threadId"] != self.thread_id
            {
                continue;
            }
            let turn = &message["params"]["turn"];
            if turn["status"] != "completed" {
                return Err(format!(
                    "Codex voice turn ended as {}: {}",
                    turn["status"], turn["error"]
                ));
            }
            let text = turn["items"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|item| item["type"] == "agentMessage" && item["phase"] == "final_answer")
                .and_then(|item| item["text"].as_str())
                .ok_or("Codex voice turn returned no final result")?;
            let result = normalize_codex_result(text)?;
            tracing::info!(
                state = %result["state"].as_str().unwrap_or("invalid"),
                intent = %result["intent"].as_str().unwrap_or("invalid"),
                message = %result["message"].as_str().unwrap_or(""),
                zone = %result["zone"].as_str().unwrap_or(""),
                heard = %result["heard"].as_str().unwrap_or(""),
                "Kizz Codex voice turn completed"
            );
            return Ok(result);
        }
        Err("Codex App Server exited before completing the voice turn".to_string())
    }

    async fn request(&mut self, mut message: Value) -> Result<u64, String> {
        let id = self.next_id;
        self.next_id += 1;
        message["id"] = json!(id);
        self.write(message).await?;
        Ok(id)
    }

    async fn notify(&mut self, message: Value) -> Result<(), String> {
        self.write(message).await
    }

    async fn write(&mut self, message: Value) -> Result<(), String> {
        let mut encoded = serde_json::to_vec(&message).map_err(|error| error.to_string())?;
        encoded.push(b'\n');
        self.stdin
            .write_all(&encoded)
            .await
            .map_err(|error| format!("Codex App Server write failed: {error}"))?;
        self.stdin
            .flush()
            .await
            .map_err(|error| format!("Codex App Server flush failed: {error}"))
    }

    async fn wait_for_response(&mut self, id: u64) -> Result<Value, String> {
        while let Some(line) = self
            .lines
            .next_line()
            .await
            .map_err(|error| format!("Codex App Server stream failed: {error}"))?
        {
            let message: Value = match serde_json::from_str(&line) {
                Ok(message) => message,
                Err(_) => continue,
            };
            if message["id"].as_u64() != Some(id) {
                continue;
            }
            if let Some(error) = message.get("error") {
                return Err(format!("Codex App Server request failed: {error}"));
            }
            return Ok(message);
        }
        Err("Codex App Server exited before responding".to_string())
    }
}

const KIZZ_AGENT_INSTRUCTIONS: &str = r#"
You are Kizz, a fast, delightful voice-controlled music companion inside a home.
The user input is a transcript from local post-wake speech recognition.
Understand it naturally; it is not an exact-command grammar and may contain
ordinary speech-recognition errors.

Use only the unified-hifi-control MCP for music state and music actions. Never
use shell, filesystem, web, code editing, or any other tool. Inspect live zones
and content when needed. Resolve natural room references and conversational
follow-ups from this persistent thread. Each turn includes current device context
with the selected zone and its title, artist, and album when something is playing.
Use the selected zone by default if the listener omitted a room. An explicitly
spoken room always overrides device context. For a clear request, perform it and
report success only when the MCP result confirms it. If the intended content or
zone is genuinely ambiguous, do not guess or act; ask one short clarification.

This is a hard execution boundary. Before considering any MCP call, classify the
transcript in the `intent` field as exactly one of `command`, `non_command`, or
`uncertain`. `command` means a clear request to control music, playback, volume,
content, or zones. `non_command` means ordinary conversation or unrelated
speech. `uncertain` means the transcript is too fragmentary or ambiguous to
establish a music request. If the intent is `non_command` or `uncertain`, do not
call any MCP tool. Return `state` as `clarify`, set `intent` accordingly, and set
`message` exactly to `I can't let you do that Dave`.

Kizz itself communicates with expressions, motion, and chirps. Do not create a
spoken model response and do not emit commentary. Return only the structured
JSON required by the supplied output schema. Keep message under 90 characters.
"#;

fn kizz_result_schema() -> Value {
    json!({
        "type":"object",
        "additionalProperties":false,
        "required":["state","message","zone","heard","intent"],
        "properties":{
            "state":{"type":"string","enum":["success","clarify","error"]},
            "message":{"type":"string"},
            "zone":{"type":["string","null"]},
            "heard":{"type":["string","null"]},
            "intent":{"type":"string","enum":["command","non_command","uncertain"]}
        }
    })
}

fn normalize_codex_result(text: &str) -> Result<Value, String> {
    let mut result: Value = serde_json::from_str(text.trim())
        .map_err(|error| format!("Codex returned invalid Kizz state: {error}"))?;
    let state = result["state"]
        .as_str()
        .ok_or("Codex returned no Kizz state")?
        .to_string();
    if !matches!(state.as_str(), "success" | "clarify" | "error") {
        return Err(format!("Codex returned unsupported Kizz state: {state}"));
    }
    if !matches!(
        result["intent"].as_str(),
        Some("command" | "non_command" | "uncertain")
    ) {
        return Err("Codex returned unsupported Kizz intent".to_string());
    }
    if result["intent"] != "command" {
        result["state"] = json!("clarify");
        result["message"] = json!("I can't let you do that Dave");
    }
    if !result["message"].is_string() {
        return Err("Codex returned no Kizz message".to_string());
    }
    result["type"] = json!("state");
    if state == "error" {
        result["state"] = json!("clarify");
    }
    Ok(result)
}

async fn send_event(socket: &mut WebSocket, event: Value) -> Result<(), axum::Error> {
    socket.send(Message::Text(event.to_string().into())).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_primary_runs_all_three_realtime_providers() {
        assert_eq!(
            providers_for(SttProvider::Deepgram),
            [
                SttProvider::Deepgram,
                SttProvider::Assemblyai,
                SttProvider::Elevenlabs,
            ]
        );
        assert_eq!(
            providers_for(SttProvider::Elevenlabs),
            [
                SttProvider::Elevenlabs,
                SttProvider::Assemblyai,
                SttProvider::Deepgram,
            ]
        );
        assert_eq!(stt_model_name(SttProvider::Deepgram), "flux-general-en");
        assert_eq!(stt_model_name(SttProvider::Assemblyai), "u3-rt-pro");
    }

    #[test]
    fn elevenlabs_committed_transcript_is_actionable() {
        let event = parse_elevenlabs_event(
            r#"{"message_type":"committed_transcript","text":"play Prince"}"#,
        )
        .expect("committed transcript event");

        match event {
            DeepgramEvent::EndOfTurn {
                transcript,
                confidence,
            } => {
                assert_eq!(transcript, "play Prince");
                assert_eq!(confidence, None);
            }
            DeepgramEvent::PartialTranscript(transcript) => {
                panic!("committed event parsed as partial: {transcript}")
            }
            DeepgramEvent::Failed(error) => panic!("unexpected failure: {error}"),
        }
    }

    #[test]
    fn elevenlabs_partial_and_final_events_do_not_commit_a_turn() {
        assert!(matches!(
            parse_elevenlabs_event(
                r#"{"message_type":"partial_transcript","text":"play Pri"}"#
            ),
            Some(DeepgramEvent::PartialTranscript(transcript)) if transcript == "play Pri"
        ));
        assert!(parse_elevenlabs_event(
            r#"{"message_type":"final_transcript","text":"play Prince"}"#
        )
        .is_none());
    }

    #[test]
    fn elevenlabs_service_errors_fail_the_provider() {
        let event = parse_elevenlabs_event(
            r#"{"message_type":"quota_exceeded","error":"quota exhausted"}"#,
        )
        .expect("error event");

        assert!(matches!(
            event,
            DeepgramEvent::Failed(error) if error.contains("quota_exceeded") && error.contains("quota exhausted")
        ));
    }

    #[test]
    fn elevenlabs_manual_commit_uses_the_official_empty_audio_message() {
        let message: Value = serde_json::from_str(&elevenlabs_audio_message(&[], true))
            .expect("manual commit message");

        assert_eq!(message["message_type"], "input_audio_chunk");
        assert_eq!(message["audio_base_64"], "");
        assert_eq!(message["commit"], true);
        assert_eq!(message["sample_rate"], 16_000);
    }

    #[test]
    fn elevenlabs_short_commands_are_padded_to_its_processing_minimum() {
        assert_eq!(elevenlabs_padding_bytes(54_272), 12_928);
        assert_eq!(elevenlabs_padding_bytes(64_000), 3_200);
        assert_eq!(elevenlabs_padding_bytes(67_200), 0);
        assert_eq!(elevenlabs_padding_bytes(80_000), 0);
    }

    #[test]
    fn device_commit_recovers_after_every_provider_finishes_without_a_winner() {
        assert!(!all_active_providers_finished(true, 2, 3));
        assert!(all_active_providers_finished(true, 3, 3));
        assert!(!all_active_providers_finished(false, 3, 3));
    }

    #[test]
    fn deepgram_close_retains_its_last_update_for_comparison_only() {
        let event = deepgram_closed_event(
            true,
            Some("What is playing right now?".to_string()),
            Some(0.82),
        );

        assert!(matches!(
            event,
            DeepgramEvent::EndOfTurn { transcript, confidence }
                if transcript == "What is playing right now?" && confidence == Some(0.82)
        ));
        assert!(matches!(
            deepgram_closed_event(false, Some("stale".to_string()), None),
            DeepgramEvent::Failed(_)
        ));
    }

    #[test]
    fn late_provider_result_cannot_enter_the_active_turn() {
        assert!(provider_event_is_current(42, 42));
        assert!(!provider_event_is_current(42, 41));
        assert!(!provider_event_is_current(42, 43));
    }

    #[test]
    fn codex_input_includes_selected_zone_and_now_playing_context() {
        let context = VoiceTurnContext {
            zone_id: Some("roon:kitchen".to_string()),
            zone_name: Some("Kitchen".to_string()),
            now_playing: Some(VoiceNowPlayingContext {
                title: "Water No Get Enemy".to_string(),
                artist: "Fela Kuti".to_string(),
                album: "Expensive Shit".to_string(),
            }),
        };

        let input = build_codex_input("turn it up", &context).expect("valid context");

        assert!(input.contains("Local speech recognition heard: turn it up"));
        assert!(input.contains(
            r#""zone_id":"roon:kitchen","zone_name":"Kitchen","now_playing":{"title":"Water No Get Enemy","artist":"Fela Kuti","album":"Expensive Shit"}"#
        ));
    }

    #[test]
    fn codex_input_marks_an_idle_selected_zone_without_stale_track_metadata() {
        let context = VoiceTurnContext {
            zone_id: Some("roon:kitchen".to_string()),
            zone_name: Some("Kitchen".to_string()),
            now_playing: None,
        };

        let input = build_codex_input("play Prince", &context).expect("valid context");

        assert!(
            input.contains(r#""zone_id":"roon:kitchen","zone_name":"Kitchen","now_playing":null"#)
        );
        assert!(!input.contains("title"));
        assert!(!input.contains("artist"));
        assert!(!input.contains("album"));
    }

    #[test]
    fn codex_success_becomes_a_kizz_state_event() {
        let result = normalize_codex_result(
            r#"{"state":"success","message":"On it.","zone":"Kitchen","heard":null,"intent":"command"}"#,
        )
        .expect("valid agent result");
        assert_eq!(result["type"], "state");
        assert_eq!(result["state"], "success");
        assert_eq!(result["zone"], "Kitchen");
    }

    #[test]
    fn codex_error_never_claims_device_success() {
        let result = normalize_codex_result(
            r#"{"state":"error","message":"Music service unavailable","zone":null,"heard":null,"intent":"command"}"#,
        )
        .expect("valid agent result");
        assert_eq!(result["state"], "clarify");
    }

    #[test]
    fn malformed_agent_output_is_rejected() {
        assert!(normalize_codex_result("played it").is_err());
    }

    #[test]
    fn codex_output_requires_a_valid_intent_classification() {
        assert!(normalize_codex_result(
            r#"{"state":"clarify","message":"Try again.","zone":null,"heard":null}"#
        )
        .is_err());
        assert!(normalize_codex_result(
            r#"{"state":"clarify","message":"Try again.","zone":null,"heard":null,"intent":"maybe"}"#
        )
        .is_err());
        let result = normalize_codex_result(
            r#"{"state":"clarify","message":"Try again.","zone":null,"heard":null,"intent":"uncertain"}"#
        )
        .expect("valid uncertain intent");
        assert_eq!(result["intent"], "uncertain");
        assert_eq!(result["state"], "clarify");
        assert_eq!(result["message"], "I can't let you do that Dave");

        let result = normalize_codex_result(
            r#"{"state":"success","message":"I changed it.","zone":"Kitchen","heard":null,"intent":"non_command"}"#
        )
        .expect("valid non-command intent");
        assert_eq!(result["state"], "clarify");
        assert_eq!(result["message"], "I can't let you do that Dave");
    }

    #[test]
    fn start_event_carries_the_devices_current_zone() {
        assert_eq!(
            parse_client_event(r#"{"type":"start","context":{"zone_id":"roon:kitchen"}}"#),
            Some(ClientEvent::Start {
                zone_id: Some("roon:kitchen".to_string())
            })
        );
    }

    #[test]
    fn event_types_are_matched_exactly() {
        assert_eq!(
            parse_client_event(r#"{"type":"commit"}"#),
            Some(ClientEvent::Commit)
        );
        assert_eq!(parse_client_event(r#"{"message":"start"}"#), None);
    }

    #[test]
    fn production_voice_rejects_training_audio() {
        assert_eq!(
            parse_client_event(r#"{"type":"wake_sample","label":"hiphi_kizz","bytes":64000}"#),
            None
        );
        assert_eq!(
            parse_client_event(r#"{"type":"wake_sample","label":"other","bytes":64000}"#),
            None
        );
        assert_eq!(
            parse_client_event(r#"{"type":"wake_sample","label":"hiphi_kizz","bytes":999999}"#),
            None
        );
    }
}
