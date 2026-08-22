//! LAN-only Kizz voice gateway.
//!
//! Kizz performs wake-word detection and VAD on-device, then streams one
//! bounded 16 kHz mono utterance. UHC streams it to speech recognition, then
//! hands the transcript to a persistent Codex App Server thread whose only
//! music capability is UHC's MCP server. Kizz owns the response character.

use axum::extract::ws::{Message, WebSocket};
use axum::extract::WebSocketUpgrade;
use axum::response::Response;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::io::Write;
use std::path::Path;
use std::process::Stdio;
use std::sync::OnceLock;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use tokio::time::{timeout, Duration};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message as DgMessage};

const SAMPLE_RATE: u32 = 16_000;
const MAX_UTTERANCE_BYTES: usize = SAMPLE_RATE as usize * 2 * 14;
const MAX_WAKE_SAMPLE_BYTES: usize = SAMPLE_RATE as usize * 2 * 3;
const MIN_UTTERANCE_BYTES: usize = SAMPLE_RATE as usize / 2;
const CODEX_START_TIMEOUT: Duration = Duration::from_secs(20);
const CODEX_TURN_TIMEOUT: Duration = Duration::from_secs(30);
const DEEPGRAM_AUDIO_CHUNK_BYTES: usize = 16_000 * 2 * 80 / 1000;

static CODEX_AGENT: OnceLock<Mutex<Option<CodexVoiceAgent>>> = OnceLock::new();

#[derive(Debug)]
enum DeepgramEvent {
    EndOfTurn {
        transcript: String,
        confidence: Option<f64>,
    },
    Failed(String),
}

struct SttTurn {
    input: mpsc::Sender<SttInput>,
}

enum SttInput {
    Audio(Vec<u8>),
    Close,
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

pub async fn voice_upgrade(upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(run_session)
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

async fn run_session(mut socket: WebSocket) {
    tracing::info!("Kizz voice session opened");
    let mut utterance = Vec::<u8>::new();
    let mut current_zone_id = None::<String>;
    let mut wake_sample = None::<WakeSample>;
    let (deepgram_events_tx, mut deepgram_events_rx) = mpsc::channel(8);
    let mut deepgram = None::<SttTurn>;
    let mut pending_fallback = None::<(Vec<u8>, Option<String>)>;
    let mut turn_completed = false;
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
                        if let Some(sample) = wake_sample.take() {
                            if pcm.len() == sample.bytes {
                                if let Err(error) = store_wake_sample(&sample.label, &pcm) {
                                    tracing::warn!(%error, "Kizz wake training sample was not stored");
                                }
                            } else {
                                tracing::warn!(expected = sample.bytes, actual = pcm.len(),
                                    "Kizz wake training sample length mismatch");
                            }
                            continue;
                        }
                        utterance.extend_from_slice(&pcm);
                        if utterance.len() > MAX_UTTERANCE_BYTES {
                            let excess = utterance.len() - MAX_UTTERANCE_BYTES;
                            utterance.drain(..excess);
                        }
                        if let Some(turn) = deepgram.as_ref() {
                            if turn.send_audio(pcm.to_vec()).await.is_err() {
                                deepgram = None;
                            }
                        }
                    }
                    Message::Text(message) => match parse_client_event(&message) {
                        Some(ClientEvent::Start { zone_id }) => {
                            utterance.clear();
                            current_zone_id = zone_id;
                            turn_completed = false;
                            pending_fallback = None;
                            deepgram = match start_stt_turn(deepgram_events_tx.clone()).await {
                                Ok(turn) => Some(turn),
                                Err(error) => {
                                    tracing::warn!(%error, "streaming speech recognition unavailable");
                                    None
                                }
                            };
                            tracing::info!(zone_id = current_zone_id.as_deref().unwrap_or("unknown"),
                                deepgram = deepgram.is_some(),
                                "Kizz voice turn received device context");
                        }
                        Some(ClientEvent::WakeSample(sample)) => wake_sample = Some(sample),
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
                                let _ = send_event(&mut socket,
                                    json!({"type":"state","state":"clarify","message":"I did not hear enough yet."})).await;
                                continue;
                            }
                            let _ = send_event(&mut socket, json!({"type":"state","state":"thinking"})).await;
                            if let Some(turn) = deepgram.as_ref() {
                                pending_fallback = Some((committed, zone_id));
                                if turn.close().await.is_ok() {
                                    tracing::info!("Deepgram Flux finalization requested by device fallback");
                                    continue;
                                }
                                deepgram = None;
                                pending_fallback = None;
                                turn_completed = true;
                                tracing::warn!("Deepgram Flux could not be finalized; no local speech recognizer is enabled");
                                let _ = send_event(&mut socket, json!({"type":"state","state":"clarify","message":"Speech recognition is unavailable. Please try once more."})).await;
                                continue;
                            }
                            turn_completed = true;
                            tracing::warn!("No streaming speech recognizer was available for the Kizz turn");
                            let _ = send_event(&mut socket, json!({"type":"state","state":"clarify","message":"Speech recognition is unavailable. Please try once more."})).await;
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
            event = deepgram_events_rx.recv() => {
                let Some(event) = event else { continue };
                match event {
                    DeepgramEvent::EndOfTurn { transcript, confidence } if !turn_completed => {
                        turn_completed = true;
                        deepgram = None;
                        let (committed, zone_id) = pending_fallback.take()
                            .unwrap_or_else(|| (std::mem::take(&mut utterance), current_zone_id.take()));
                        tracing::info!(%transcript, confidence, bytes = committed.len(),
                            "Deepgram Flux ended Kizz voice turn");
                        let _ = send_event(&mut socket, json!({"type":"endpoint","reason":"deepgram_flux","confidence":confidence})).await;
                        let _ = send_event(&mut socket, json!({"type":"state","state":"thinking"})).await;
                        let result = match codex_transcript_request(&transcript, zone_id.as_deref()).await {
                            Ok(result) => result,
                            Err(error) => {
                                tracing::warn!(%error, "Kizz Codex voice turn failed");
                                json!({"type":"state","state":"clarify","message":"I lost that thought. Please try once more."})
                            }
                        };
                        let _ = send_event(&mut socket, result).await;
                    }
                    DeepgramEvent::EndOfTurn { .. } => {}
                    DeepgramEvent::Failed(error) => {
                        deepgram = None;
                        if pending_fallback.take().is_some() {
                            tracing::warn!(%error, "Deepgram Flux finalization failed; no local speech recognizer is enabled");
                            turn_completed = true;
                            let _ = send_event(&mut socket, json!({"type":"state","state":"clarify","message":"Speech recognition is unavailable. Please try once more."})).await;
                        } else {
                            tracing::warn!(%error, "Deepgram Flux stream failed; awaiting device fallback endpoint");
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
    WakeSample(WakeSample),
    Commit,
}

#[derive(Debug, PartialEq)]
struct WakeSample {
    label: String,
    bytes: usize,
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
        "wake_sample" => {
            let label = event.get("label")?.as_str()?;
            let bytes = usize::try_from(event.get("bytes")?.as_u64()?).ok()?;
            if label != "hiphi_kizz" || bytes == 0 || bytes > MAX_WAKE_SAMPLE_BYTES {
                return None;
            }
            Some(ClientEvent::WakeSample(WakeSample {
                label: label.to_owned(),
                bytes,
            }))
        }
        "commit" => Some(ClientEvent::Commit),
        _ => None,
    }
}

fn store_wake_sample(label: &str, pcm: &[u8]) -> Result<(), String> {
    let Some(root) = std::env::var_os("KIZZ_WAKE_TRAINING_DIR") else {
        return Ok(());
    };
    let destination = Path::new(&root).join("positive").join(label);
    std::fs::create_dir_all(&destination).map_err(|error| error.to_string())?;
    let mut sample = tempfile::Builder::new()
        .prefix("device-")
        .suffix(".wav")
        .tempfile_in(&destination)
        .map_err(|error| error.to_string())?;
    write_wav(&mut sample, pcm).map_err(|error| error.to_string())?;
    let path = sample
        .into_temp_path()
        .keep()
        .map_err(|error| error.to_string())?;
    tracing::info!(path = %path.display(), bytes = pcm.len(),
        "Stored Kizz wake training sample");
    Ok(())
}

async fn start_stt_turn(events: mpsc::Sender<DeepgramEvent>) -> Result<SttTurn, String> {
    match std::env::var("KIZZ_STT_PROVIDER")
        .unwrap_or_else(|_| "deepgram".to_string())
        .as_str()
    {
        "assemblyai" | "assembly" => start_assemblyai_turn(events).await,
        _ => start_deepgram_turn(events).await,
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
                                    let _ = events.send(DeepgramEvent::Failed(error.to_string())).await;
                                    return;
                                }
                            }
                        }
                        Some(SttInput::Close) | None => {
                            input_open = false;
                            if !pending.is_empty() {
                                if let Err(error) = output.send(DgMessage::Binary(
                                    std::mem::take(&mut pending))).await {
                                    let _ = events.send(DeepgramEvent::Failed(error.to_string())).await;
                                    return;
                                }
                            }
                            if let Err(error) = output.send(DgMessage::Text(
                                json!({"type":"CloseStream"}).to_string())).await {
                                let _ = events.send(DeepgramEvent::Failed(error.to_string())).await;
                                return;
                            }
                            close_deadline = Some(tokio::time::Instant::now() + Duration::from_secs(3));
                        }
                    }
                }
                message = input.next() => {
                    let Some(message) = message else {
                        let _ = events.send(DeepgramEvent::Failed(
                            "Deepgram closed the stream".to_string())).await;
                        return;
                    };
                    match message {
                        Ok(DgMessage::Text(text)) => {
                            let Ok(event) = serde_json::from_str::<Value>(&text) else { continue };
                            if event.get("type").and_then(Value::as_str) == Some("Error") {
                                let _ = events.send(DeepgramEvent::Failed(event.to_string())).await;
                                return;
                            }
                            if event.get("type").and_then(Value::as_str) == Some("TurnInfo") &&
                                event.get("event").and_then(Value::as_str) == Some("EndOfTurn") {
                                let transcript = event.get("transcript")
                                    .and_then(Value::as_str).unwrap_or("").trim().to_string();
                                let confidence = event.get("end_of_turn_confidence")
                                    .and_then(Value::as_f64);
                                if transcript.is_empty() {
                                    let _ = events.send(DeepgramEvent::Failed(
                                        "Deepgram ended an empty turn".to_string())).await;
                                } else {
                                    let _ = events.send(DeepgramEvent::EndOfTurn {
                                        transcript,
                                        confidence,
                                    }).await;
                                }
                                return;
                            }
                        }
                        Ok(DgMessage::Close(_)) => {
                            let _ = events.send(DeepgramEvent::Failed(
                                "Deepgram closed the stream".to_string())).await;
                            return;
                        }
                        Err(error) => {
                            let _ = events.send(DeepgramEvent::Failed(error.to_string())).await;
                            return;
                        }
                        _ => {}
                    }
                }
                _ = tokio::time::sleep_until(close_deadline.unwrap_or_else(||
                    tokio::time::Instant::now() + Duration::from_secs(86_400))),
                    if close_deadline.is_some() => {
                    let _ = events.send(DeepgramEvent::Failed(
                        "Deepgram finalization timed out".to_string())).await;
                    return;
                }
            }
        }
    });
    Ok(SttTurn { input: input_tx })
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
        loop {
            tokio::select! {
                command = input_rx.recv() => {
                    match command {
                        Some(SttInput::Audio(audio)) => {
                            if let Err(error) = output.send(DgMessage::Binary(audio)).await {
                                let _ = events.send(DeepgramEvent::Failed(error.to_string())).await;
                                return;
                            }
                        }
                        Some(SttInput::Close) | None => {
                            if let Err(error) = output.send(DgMessage::Text(
                                json!({"type":"Terminate"}).to_string())).await {
                                let _ = events.send(DeepgramEvent::Failed(error.to_string())).await;
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
                                                let _ = events.send(DeepgramEvent::EndOfTurn { transcript, confidence }).await;
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
                                Ok(Err(error)) => { let _ = events.send(DeepgramEvent::Failed(error)).await; }
                                Err(_) => { let _ = events.send(DeepgramEvent::Failed("AssemblyAI finalization timed out".to_string())).await; }
                            }
                            return;
                        }
                    }
                }
                message = input.next() => {
                    let Some(message) = message else {
                        let _ = events.send(DeepgramEvent::Failed("AssemblyAI closed the stream".to_string())).await;
                        return;
                    };
                    match message {
                        Ok(DgMessage::Text(text)) => {
                            let Ok(event) = serde_json::from_str::<Value>(&text) else { continue };
                            if event.get("type").and_then(Value::as_str) == Some("Error") {
                                let _ = events.send(DeepgramEvent::Failed(event.to_string())).await;
                                return;
                            }
                            if event.get("type").and_then(Value::as_str) == Some("Turn") &&
                                event.get("end_of_turn").and_then(Value::as_bool) == Some(true) {
                                let transcript = event.get("transcript").and_then(Value::as_str).unwrap_or("").trim().to_string();
                                if transcript.is_empty() {
                                    let _ = events.send(DeepgramEvent::Failed("AssemblyAI ended an empty turn".to_string())).await;
                                } else {
                                    let confidence = event.get("end_of_turn_confidence").and_then(Value::as_f64);
                                    let _ = events.send(DeepgramEvent::EndOfTurn { transcript, confidence }).await;
                                }
                                return;
                            }
                        }
                        Ok(DgMessage::Close(_)) => {
                            let _ = events.send(DeepgramEvent::Failed("AssemblyAI closed the stream".to_string())).await;
                            return;
                        }
                        Err(error) => {
                            let _ = events.send(DeepgramEvent::Failed(error.to_string())).await;
                            return;
                        }
                        _ => {}
                    }
                }
            }
        }
    });
    Ok(SttTurn { input: input_tx })
}

async fn codex_transcript_request(
    transcript: &str,
    current_zone_id: Option<&str>,
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
        .turn(transcript, current_zone_id);
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
    let result = timeout(
        CODEX_TURN_TIMEOUT,
        restarted.turn(transcript, current_zone_id),
    )
    .await
    .map_err(|_| "Codex voice turn timed out after restart".to_string())?;
    *_conversation_guard = Some(restarted);
    result
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
        current_zone_id: Option<&str>,
    ) -> Result<Value, String> {
        let zone_context = current_zone_id
            .map(|zone_id| {
                format!(
                    "\nKizz's currently selected zone id is: {zone_id}. Use it as the default only when the listener did not name a room."
                )
            })
            .unwrap_or_default();
        let request_id = self
            .request(json!({
                "method":"turn/start",
                "params": {
                    "threadId": self.thread_id,
                    "input": [{
                        "type":"text",
                        "text": format!("Local speech recognition heard: {transcript}{zone_context}")
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
follow-ups from this persistent thread. When a current zone id is supplied, use
that zone by default if the listener omitted a room. An explicitly spoken room
always overrides device context. For a clear request, perform it and
report success only when the MCP result confirms it. If the intended content or
zone is genuinely ambiguous, do not guess or act; ask one short clarification.

Kizz itself communicates with expressions, motion, and chirps. Do not create a
spoken model response and do not emit commentary. Return only the structured
JSON required by the supplied output schema. Keep message under 90 characters.
"#;

fn kizz_result_schema() -> Value {
    json!({
        "type":"object",
        "additionalProperties":false,
        "required":["state","message","zone","heard"],
        "properties":{
            "state":{"type":"string","enum":["success","clarify","error"]},
            "message":{"type":"string"},
            "zone":{"type":["string","null"]},
            "heard":{"type":["string","null"]}
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
    if !result["message"].is_string() {
        return Err("Codex returned no Kizz message".to_string());
    }
    result["type"] = json!("state");
    if state == "error" {
        result["state"] = json!("clarify");
    }
    Ok(result)
}

fn write_wav(file: &mut tempfile::NamedTempFile, pcm: &[u8]) -> std::io::Result<()> {
    let data_len = pcm.len() as u32;
    file.write_all(b"RIFF")?;
    file.write_all(&(36 + data_len).to_le_bytes())?;
    file.write_all(b"WAVEfmt ")?;
    file.write_all(&16u32.to_le_bytes())?;
    file.write_all(&1u16.to_le_bytes())?;
    file.write_all(&1u16.to_le_bytes())?;
    file.write_all(&SAMPLE_RATE.to_le_bytes())?;
    file.write_all(&(SAMPLE_RATE * 2).to_le_bytes())?;
    file.write_all(&2u16.to_le_bytes())?;
    file.write_all(&16u16.to_le_bytes())?;
    file.write_all(b"data")?;
    file.write_all(&data_len.to_le_bytes())?;
    file.write_all(pcm)?;
    file.flush()
}

async fn send_event(socket: &mut WebSocket, event: Value) -> Result<(), axum::Error> {
    socket.send(Message::Text(event.to_string().into())).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_success_becomes_a_kizz_state_event() {
        let result = normalize_codex_result(
            r#"{"state":"success","message":"On it.","zone":"Kitchen","heard":null}"#,
        )
        .expect("valid agent result");
        assert_eq!(result["type"], "state");
        assert_eq!(result["state"], "success");
        assert_eq!(result["zone"], "Kitchen");
    }

    #[test]
    fn codex_error_never_claims_device_success() {
        let result = normalize_codex_result(
            r#"{"state":"error","message":"Music service unavailable","zone":null,"heard":null}"#,
        )
        .expect("valid agent result");
        assert_eq!(result["state"], "clarify");
    }

    #[test]
    fn malformed_agent_output_is_rejected() {
        assert!(normalize_codex_result("played it").is_err());
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
    fn wake_training_sample_is_bounded_and_exactly_labeled() {
        assert_eq!(
            parse_client_event(r#"{"type":"wake_sample","label":"hiphi_kizz","bytes":64000}"#),
            Some(ClientEvent::WakeSample(WakeSample {
                label: "hiphi_kizz".to_string(),
                bytes: 64_000,
            }))
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
