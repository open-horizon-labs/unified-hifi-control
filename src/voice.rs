//! LAN-only Kizz voice gateway.
//!
//! Kizz performs wake-word detection and VAD on-device, then streams one
//! bounded 16 kHz mono utterance. UHC wraps the PCM as WAV and hands it to a
//! persistent Codex App Server thread whose only music capability is UHC's
//! MCP server. Kizz, not the model, owns the audible and visual response.

use crate::config::get_data_dir;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::WebSocketUpgrade;
use axum::response::Response;
use serde_json::{json, Value};
use std::io::Write;
use std::path::Path;
use std::process::Stdio;
use std::sync::OnceLock;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::time::{timeout, Duration};

const SAMPLE_RATE: u32 = 16_000;
const MAX_UTTERANCE_BYTES: usize = SAMPLE_RATE as usize * 2 * 14;
const MAX_WAKE_SAMPLE_BYTES: usize = SAMPLE_RATE as usize * 2 * 3;
const MIN_UTTERANCE_BYTES: usize = SAMPLE_RATE as usize / 2;
const CODEX_START_TIMEOUT: Duration = Duration::from_secs(20);
const CODEX_TURN_TIMEOUT: Duration = Duration::from_secs(30);
const TRANSCRIBE_TIMEOUT: Duration = Duration::from_secs(15);

static CODEX_AGENT: OnceLock<Mutex<Option<CodexVoiceAgent>>> = OnceLock::new();

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
    while let Some(incoming) = socket.recv().await {
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
                        tracing::warn!(
                            expected = sample.bytes,
                            actual = pcm.len(),
                            "Kizz wake training sample length mismatch"
                        );
                    }
                    continue;
                }
                utterance.extend_from_slice(&pcm);
                if utterance.len() > MAX_UTTERANCE_BYTES {
                    let excess = utterance.len() - MAX_UTTERANCE_BYTES;
                    utterance.drain(..excess);
                }
            }
            Message::Text(message) => match parse_client_event(&message) {
                Some(ClientEvent::Start { zone_id }) => {
                    utterance.clear();
                    current_zone_id = zone_id;
                    tracing::info!(
                        zone_id = current_zone_id.as_deref().unwrap_or("unknown"),
                        "Kizz voice turn received device context"
                    );
                }
                Some(ClientEvent::WakeSample(sample)) => {
                    wake_sample = Some(sample);
                }
                Some(ClientEvent::Commit) => {
                    let committed = std::mem::take(&mut utterance);
                    let zone_id = current_zone_id.take();
                    tracing::info!(bytes = committed.len(), "Kizz utterance committed");
                    if committed.len() < MIN_UTTERANCE_BYTES {
                        let _ = send_event(
                        &mut socket,
                        json!({"type":"state","state":"clarify","message":"I did not hear enough yet."}),
                    )
                    .await;
                        continue;
                    }

                    // The sentence has ended, so Kizz may leave listening and show
                    // thinking while the same bounded audio is reasoned over.
                    let _ =
                        send_event(&mut socket, json!({"type":"state","state":"thinking"})).await;

                    let result = match codex_request(&committed, zone_id.as_deref()).await {
                        Ok(result) => result,
                        Err(error) => {
                            tracing::warn!(%error, "Kizz Codex voice turn failed");
                            json!({"type":"state","state":"clarify","message":"I lost that thought. Please try once more."})
                        }
                    };
                    let _ = send_event(&mut socket, result).await;
                }
                None => tracing::warn!(%message, "Ignored unknown Kizz voice event"),
            },
            Message::Ping(payload) => {
                if socket.send(Message::Pong(payload)).await.is_err() {
                    break;
                }
            }
            Message::Close(_) => {
                tracing::info!("Kizz voice session closed by client");
                break;
            }
            _ => {}
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

async fn codex_request(pcm: &[u8], current_zone_id: Option<&str>) -> Result<Value, String> {
    let (conditioned, capture_peak, gain) = condition_voice_pcm(pcm);
    tracing::info!(
        capture_peak,
        gain = format_args!("{gain:.1}"),
        "Conditioned Kizz voice capture for the audio model"
    );
    let mut wav = tempfile::Builder::new()
        .prefix("kizz-utterance-")
        .suffix(".wav")
        .tempfile()
        .map_err(|error| error.to_string())?;
    write_wav(&mut wav, &conditioned).map_err(|error| error.to_string())?;
    let transcript = transcribe_voice(wav.path()).await?;
    tracing::info!(%transcript, "Kizz local speech recognition completed");

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
        .turn(&transcript, current_zone_id);
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
        restarted.turn(&transcript, current_zone_id),
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

fn condition_voice_pcm(pcm: &[u8]) -> (Vec<u8>, i32, f32) {
    let peak = pcm
        .chunks_exact(2)
        .map(|sample| i16::from_le_bytes([sample[0], sample[1]]) as i32)
        .map(i32::abs)
        .max()
        .unwrap_or(0);
    if peak == 0 {
        return (pcm.to_vec(), 0, 1.0);
    }

    // CoreS3's official microphone path is intentionally conservative. Raise
    // only the captured utterance to a healthy model input level; this never
    // touches M5Unified speaker gain or Kizz's chirp loudness.
    let gain = (18_000.0 / peak as f32).clamp(1.0, 16.0);
    let mut output = Vec::with_capacity(pcm.len());
    for sample in pcm.chunks_exact(2) {
        let value = i16::from_le_bytes([sample[0], sample[1]]) as f32;
        let conditioned = (value * gain).round().clamp(-32_768.0, 32_767.0) as i16;
        output.extend_from_slice(&conditioned.to_le_bytes());
    }
    (output, peak, gain)
}

async fn transcribe_voice(wav_path: &Path) -> Result<String, String> {
    let model = std::env::var_os("KIZZ_WHISPER_MODEL")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| get_data_dir().join("models/ggml-base.en.bin"));
    if !model.is_file() {
        return Err(format!(
            "Kizz Whisper model is missing at {} (set KIZZ_WHISPER_MODEL to override)",
            model.display()
        ));
    }
    let output = timeout(
        TRANSCRIBE_TIMEOUT,
        Command::new("whisper-cli")
            .args([
                "-m",
                model
                    .to_str()
                    .ok_or("Kizz Whisper model path is not valid UTF-8")?,
                "-f",
                wav_path
                    .to_str()
                    .ok_or("Kizz WAV path is not valid UTF-8")?,
                "-l",
                "en",
                "-t",
                "4",
                "--no-timestamps",
                "--no-prints",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output(),
    )
    .await
    .map_err(|_| "Kizz local speech recognition timed out".to_string())?
    .map_err(|error| format!("Kizz local speech recognition is unavailable: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "Kizz local speech recognition exited as {}",
            output.status
        ));
    }
    let transcript = String::from_utf8(output.stdout)
        .map_err(|error| format!("Kizz transcript was not UTF-8: {error}"))?
        .trim()
        .to_string();
    if transcript.is_empty() {
        return Err("Kizz local speech recognition heard no words".to_string());
    }
    Ok(transcript)
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

    #[test]
    fn quiet_voice_capture_is_raised_without_touching_duration() {
        let pcm = [500i16.to_le_bytes(), (-750i16).to_le_bytes()].concat();
        let (conditioned, peak, gain) = condition_voice_pcm(&pcm);
        assert_eq!(peak, 750);
        assert_eq!(gain, 16.0);
        assert_eq!(conditioned.len(), pcm.len());
        assert_eq!(i16::from_le_bytes([conditioned[0], conditioned[1]]), 8_000);
        assert_eq!(
            i16::from_le_bytes([conditioned[2], conditioned[3]]),
            -12_000
        );
    }
}
