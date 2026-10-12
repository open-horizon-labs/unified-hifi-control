//! Mock Denon/Marantz receiver: Telnet (line protocol) and the `goform` HTTP status document,
//! on ephemeral ports.
//!
//! Replies are shaped like the SR6008 captures in `tests/fixtures/devices/marantz/sr6008/`:
//! every Telnet reply is followed by an interleaved `@`-protocol line, `MV?` is answered with the
//! level and then `MVMAX`, and the status document carries the same tags. Synthetic behaviour —
//! not hardware evidence.
//!
//! Every byte the mock receives is recorded, so a test can assert what was (and was not) sent.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, Mutex};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct MockMarantzState {
    pub zone_on: bool,
    /// Level on the absolute `MV` scale (dB + 80), in half steps.
    pub level_halves: u32,
    /// `MVMAX` on the absolute scale.
    pub max_absolute: u32,
    pub muted: bool,
    pub input: String,
    /// Reply to a write whose value equals the current one (the hardware behaviour is not yet
    /// captured; tests choose).
    pub echo_noop_writes: bool,
    /// When false the unit accepts writes but never answers them — not even queries.
    pub answers: bool,
    /// Every line received over Telnet, in order, including queries.
    pub telnet_received: Vec<String>,
    /// Every HTTP path requested, in order.
    pub http_requests: Vec<String>,
}

impl Default for MockMarantzState {
    fn default() -> Self {
        Self {
            zone_on: true,
            level_halves: 81, // 40.5 absolute = -39.5 dB, as captured
            max_absolute: 70, // -10 dB, as captured
            muted: false,
            input: "MPLAY".to_string(),
            echo_noop_writes: true,
            answers: true,
            telnet_received: Vec::new(),
            http_requests: Vec::new(),
        }
    }
}

impl MockMarantzState {
    pub fn level_db(&self) -> f32 {
        self.level_halves as f32 / 2.0 - 80.0
    }

    fn mv(&self) -> String {
        let whole = self.level_halves / 2;
        if self.level_halves % 2 == 1 {
            format!("MV{whole:02}5")
        } else {
            format!("MV{whole:02}")
        }
    }

    /// Lines that set state (not queries), which is what a safety test inspects.
    pub fn writes(&self) -> Vec<String> {
        self.telnet_received
            .iter()
            .filter(|line| !line.ends_with('?'))
            .cloned()
            .collect()
    }

    fn status_xml(&self) -> String {
        format!(
            r#"<?xml version="1.0" encoding="utf-8" ?>
<item>
<Zone><value>MAIN ZONE </value></Zone>
<Power><value>{power}</value></Power>
<InputFuncList>
<value>CD</value>
<value>TV AUDIO</value>
<value>Media Player</value>
</InputFuncList>
<RenameSource>
<value><value>CD          </value></value>
<value><value>TV AUDIO    </value></value>
<value><value>WiiM        </value></value>
</RenameSource>
<SourceDelete>
<value>DEL</value>
<value>USE</value>
<value>USE</value>
</SourceDelete>
<InputFuncSelect><value>{input}</value></InputFuncSelect>
<VolumeDisplay><value>Absolute</value></VolumeDisplay>
<MasterVolume><value>{level:.1}</value></MasterVolume>
<Mute><value>{mute}</value></Mute>
<Model><value>60XX E3</value></Model>
</item>
"#,
            power = if self.zone_on { "ON" } else { "OFF" },
            input = self.input,
            level = self.level_db(),
            mute = if self.muted { "on" } else { "off" },
        )
    }
}

pub struct MockMarantz {
    pub telnet_addr: SocketAddr,
    pub http_addr: SocketAddr,
    pub state: Arc<Mutex<MockMarantzState>>,
    push: broadcast::Sender<String>,
    shutdown: CancellationToken,
}

impl MockMarantz {
    pub async fn start() -> Self {
        Self::start_with(MockMarantzState::default()).await
    }

    pub async fn start_with(initial: MockMarantzState) -> Self {
        let telnet = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let telnet_addr = telnet.local_addr().unwrap();
        let http_addr = http.local_addr().unwrap();
        let state = Arc::new(Mutex::new(initial));
        let shutdown = CancellationToken::new();
        let (push, _) = broadcast::channel(16);

        {
            let state = state.clone();
            let shutdown = shutdown.clone();
            let push = push.clone();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = shutdown.cancelled() => break,
                        accepted = telnet.accept() => {
                            let Ok((stream, _)) = accepted else { continue };
                            let state = state.clone();
                            let shutdown = shutdown.clone();
                            let pushed = push.subscribe();
                            tokio::spawn(async move { serve_telnet(stream, state, pushed, shutdown).await });
                        }
                    }
                }
            });
        }
        {
            let state = state.clone();
            let shutdown = shutdown.clone();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = shutdown.cancelled() => break,
                        accepted = http.accept() => {
                            let Ok((stream, _)) = accepted else { continue };
                            let state = state.clone();
                            tokio::spawn(async move { serve_http(stream, state).await });
                        }
                    }
                }
            });
        }

        Self {
            telnet_addr,
            http_addr,
            state,
            push,
            shutdown,
        }
    }

    /// Push an arbitrary line to the connected Telnet client, as a firmware variant might.
    pub async fn push_raw(&self, line: &str) {
        if self.push.send(format!("{line}\r")).is_err() {
            // No connected client.
        }
    }

    /// Someone turned the receiver's own volume knob or remote: the level changes and the unit
    /// pushes the new `MV` line, unsolicited, to the connected Telnet client.
    pub async fn remote_set_level(&self, level_db: f32) {
        let line = {
            let mut state = self.state.lock().await;
            state.level_halves = ((level_db + 80.0) * 2.0).round() as u32;
            format!("{}\r@VOL:x\r", state.mv())
        };
        if self.push.send(line).is_err() {
            // No connected client: the level still changed; HTTP polling will see it.
        }
    }

    pub fn ports(&self) -> (u16, u16) {
        (self.telnet_addr.port(), self.http_addr.port())
    }

    pub async fn snapshot(&self) -> MockMarantzState {
        self.state.lock().await.clone()
    }
}

impl Drop for MockMarantz {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

async fn serve_telnet(
    mut stream: TcpStream,
    state: Arc<Mutex<MockMarantzState>>,
    mut pushed: broadcast::Receiver<String>,
    shutdown: CancellationToken,
) {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 256];
    loop {
        let read = tokio::select! {
            _ = shutdown.cancelled() => return,
            line = pushed.recv() => {
                if let Ok(line) = line {
                    if stream.write_all(line.as_bytes()).await.is_err() {
                        return;
                    }
                }
                continue;
            }
            read = stream.read(&mut chunk) => read,
        };
        let n = match read {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        buffer.extend_from_slice(&chunk[..n]);
        while let Some(end) = buffer.iter().position(|b| *b == b'\r') {
            let line: Vec<u8> = buffer.drain(..=end).collect();
            let line = String::from_utf8_lossy(&line).trim().to_string();
            if line.is_empty() {
                continue;
            }
            let reply = {
                let mut state = state.lock().await;
                state.telnet_received.push(line.clone());
                if state.answers {
                    respond(&mut state, &line)
                } else {
                    String::new()
                }
            };
            if !reply.is_empty() && stream.write_all(reply.as_bytes()).await.is_err() {
                return;
            }
        }
    }
}

/// The receiver's reply to one line, mirroring the captured reply shapes.
fn respond(state: &mut MockMarantzState, line: &str) -> String {
    match line {
        "PW?" => format!(
            "PW{}\r@PWR:2\r",
            if state.zone_on { "ON" } else { "STANDBY" }
        ),
        "ZM?" => format!("ZM{}\r@PWR:2\r", if state.zone_on { "ON" } else { "OFF" }),
        "MV?" => format!("{}\r@VOL:x\rMVMAX {}\r", state.mv(), state.max_absolute),
        "MU?" => format!("MU{}\r@AMT:1\r", if state.muted { "ON" } else { "OFF" }),
        "SI?" => format!("SI{}\r@SRC:33\r", state.input),
        "ZMON" | "ZMOFF" => {
            let on = line == "ZMON";
            let changed = state.zone_on != on;
            state.zone_on = on;
            echo(
                state,
                changed,
                format!("ZM{}\r", if on { "ON" } else { "OFF" }),
            )
        }
        "MUON" | "MUOFF" => {
            let muted = line == "MUON";
            let changed = state.muted != muted;
            state.muted = muted;
            echo(
                state,
                changed,
                format!("MU{}\r", if muted { "ON" } else { "OFF" }),
            )
        }
        _ if line.starts_with("SI") => {
            let input = line[2..].to_string();
            let changed = state.input != input;
            state.input = input;
            echo(state, changed, format!("SI{}\r", state.input))
        }
        _ if line.starts_with("MV") && line[2..].bytes().all(|b| b.is_ascii_digit()) => {
            let digits = &line[2..];
            let whole: u32 = digits[..2].parse().unwrap_or(0);
            let half = u32::from(digits.len() == 3 && digits.ends_with('5'));
            // The receiver enforces its own maximum, as the real unit does.
            let halves = (whole * 2 + half).min(state.max_absolute * 2);
            let changed = state.level_halves != halves;
            state.level_halves = halves;
            echo(state, changed, format!("{}\r", state.mv()))
        }
        _ => String::new(),
    }
}

fn echo(state: &MockMarantzState, changed: bool, reply: String) -> String {
    if changed || state.echo_noop_writes {
        reply
    } else {
        String::new()
    }
}

async fn serve_http(mut stream: TcpStream, state: Arc<Mutex<MockMarantzState>>) {
    let mut request = Vec::new();
    let mut chunk = [0u8; 1024];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
        match tokio::time::timeout_at(deadline, stream.read(&mut chunk)).await {
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => return,
            Ok(Ok(n)) => request.extend_from_slice(&chunk[..n]),
        }
    }
    let text = String::from_utf8_lossy(&request);
    let path = text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/")
        .to_string();
    let body = {
        let mut state = state.lock().await;
        state.http_requests.push(path.clone());
        if path.starts_with("/goform/formMainZone_MainZoneXmlStatus.xml") {
            Some(state.status_xml())
        } else if path.starts_with("/goform/formiPhoneAppPower.xml?1+PowerOn") {
            state.zone_on = true;
            Some(String::new())
        } else if path.starts_with("/goform/formiPhoneAppPower.xml?1+PowerStandby") {
            state.zone_on = false;
            Some(String::new())
        } else {
            None
        }
    };
    let response = match body {
        Some(body) => format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        ),
        None => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
    };
    if stream.write_all(response.as_bytes()).await.is_err() {
        return;
    }
    if stream.shutdown().await.is_err() {
        // The client may already have closed; nothing to recover.
    }
}
