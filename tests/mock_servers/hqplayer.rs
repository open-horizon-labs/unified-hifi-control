#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Mock HQPlayer for testing
//!
//! Simulates the TCP/XML protocol on port 4321

use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

/// Mock HQPlayer state
#[derive(Debug, Clone)]
pub struct MockHqpState {
    pub state: u8, // 0=stopped, 1=paused, 2=playing
    pub mode: u8,  // 0=PCM, 1=SDM
    pub filter: u32,
    pub shaper: u32,
    pub rate: u32,
    pub volume: i32, // dB value
    pub track_title: String,
    pub track_artist: String,
    pub track_album: String,
    pub position: u32,
    pub length: u32,
}

impl Default for MockHqpState {
    fn default() -> Self {
        Self {
            state: 0,
            mode: 0,
            filter: 0,
            shaper: 0,
            rate: 0,
            volume: -20,
            track_title: String::new(),
            track_artist: String::new(),
            track_album: String::new(),
            position: 0,
            length: 0,
        }
    }
}

/// Mock HQPlayer server
pub struct MockHqpServer {
    addr: SocketAddr,
    state: Arc<RwLock<MockHqpState>>,
    handle: JoinHandle<()>,
}

impl MockHqpServer {
    /// Start a mock HQPlayer server on a random port
    pub async fn start() -> Self {
        let state = Arc::new(RwLock::new(MockHqpState::default()));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let state_clone = state.clone();
        let handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let state = state_clone.clone();
                        tokio::spawn(async move {
                            handle_connection(stream, state).await;
                        });
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            addr,
            state,
            handle,
        }
    }

    /// Get the server address
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Set playback state (0=stopped, 1=paused, 2=playing)
    pub async fn set_state(&self, state: u8) {
        self.state.write().await.state = state;
    }

    /// Set volume (dB)
    pub async fn set_volume(&self, volume: i32) {
        self.state.write().await.volume = volume;
    }

    /// Set now playing info
    pub async fn set_now_playing(&self, title: &str, artist: &str, album: &str, length: u32) {
        let mut state = self.state.write().await;
        state.track_title = title.to_string();
        state.track_artist = artist.to_string();
        state.track_album = album.to_string();
        state.length = length;
    }

    /// Stop the mock server
    pub async fn stop(self) {
        self.handle.abort();
    }
}

/// Handle a single TCP connection
async fn handle_connection(stream: TcpStream, state: Arc<RwLock<MockHqpState>>) {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break, // Connection closed
            Ok(_) => {
                let response = process_command(&line, &state).await;
                if writer.write_all(response.as_bytes()).await.is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

/// Process an XML command and return a response
async fn process_command(command: &str, state: &Arc<RwLock<MockHqpState>>) -> String {
    // Skip XML declaration
    let command = command.trim();
    if command.starts_with("<?xml") {
        return String::new(); // Ignore declaration line
    }

    // Parse command name from XML
    let cmd_name = parse_element_name(command);

    let state = state.read().await;

    match cmd_name.as_str() {
        "GetInfo" => format!(
            "<?xml version=\"1.0\"?>\n<GetInfo name=\"MockHQPlayer\" product=\"HQPlayer\" version=\"5.0.0\" platform=\"mock\" engine=\"mock\"/>\n"
        ),
        "State" => format!(
            "<?xml version=\"1.0\"?>\n<State state=\"{}\" mode=\"{}\" filter=\"{}\" shaper=\"{}\" rate=\"{}\" volume=\"{}\"/>\n",
            state.state, state.mode, state.filter, state.shaper, state.rate, state.volume
        ),
        "Status" => format!(
            "<?xml version=\"1.0\"?>\n<Status state=\"{}\" track=\"0\" track_id=\"\" position=\"{}\" length=\"{}\" volume=\"{}\" active_mode=\"PCM\" active_filter=\"poly-sinc-xtr\" active_shaper=\"NS9\" active_rate=\"352800\"/>\n",
            state.state, state.position, state.length, state.volume
        ),
        "VolumeRange" => {
            "<?xml version=\"1.0\"?>\n<VolumeRange min=\"-60\" max=\"0\" step=\"1\" enabled=\"1\" adaptive=\"0\"/>\n".to_string()
        }
        "GetModes" => {
            "<?xml version=\"1.0\"?>\n<GetModes><ModesItem index=\"0\" name=\"PCM\" value=\"0\"/><ModesItem index=\"1\" name=\"SDM\" value=\"1\"/></GetModes>\n".to_string()
        }
        "GetFilters" => {
            "<?xml version=\"1.0\"?>\n<GetFilters><FiltersItem index=\"0\" name=\"poly-sinc-xtr\" value=\"0\" arg=\"0\"/><FiltersItem index=\"1\" name=\"closed-form\" value=\"1\" arg=\"0\"/></GetFilters>\n".to_string()
        }
        "GetShapers" => {
            "<?xml version=\"1.0\"?>\n<GetShapers><ShapersItem index=\"0\" name=\"NS9\" value=\"0\"/><ShapersItem index=\"1\" name=\"NS5\" value=\"1\"/></GetShapers>\n".to_string()
        }
        "GetRates" => {
            "<?xml version=\"1.0\"?>\n<GetRates><RatesItem index=\"0\" rate=\"352800\"/><RatesItem index=\"1\" rate=\"705600\"/></GetRates>\n".to_string()
        }
        "MatrixListProfiles" => {
            "<?xml version=\"1.0\"?>\n<MatrixListProfiles><MatrixProfile index=\"0\" name=\"Default\"/><MatrixProfile index=\"1\" name=\"Night\"/></MatrixListProfiles>\n".to_string()
        }
        "MatrixGetProfile" => {
            "<?xml version=\"1.0\"?>\n<MatrixGetProfile index=\"0\" value=\"Default\"/>\n".to_string()
        }
        // Control commands - return empty acknowledgment
        "Play" | "Pause" | "Stop" | "Previous" | "Next" | "Seek" |
        "SetMode" | "SetFilter" | "SetShaping" | "SetRate" | "Volume" |
        "VolumeUp" | "VolumeDown" | "VolumeMute" | "MatrixSetProfile" => {
            "<?xml version=\"1.0\"?>\n<Ok/>\n".to_string()
        }
        _ => {
            format!("<?xml version=\"1.0\"?>\n<Error message=\"Unknown command: {}\"/>\n", cmd_name)
        }
    }
}

/// Parse element name from XML like "<GetInfo attr="val"/>"
fn parse_element_name(xml: &str) -> String {
    let xml = xml.trim();
    if xml.starts_with('<') {
        let end = xml[1..]
            .find(|c: char| c.is_whitespace() || c == '/' || c == '>')
            .unwrap_or(xml.len() - 1);
        xml[1..=end].to_string()
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn mock_hqp_starts_and_stops() {
        let server = MockHqpServer::start().await;
        let addr = server.addr();
        assert!(addr.port() > 0);
        server.stop().await;
    }

    #[tokio::test]
    async fn mock_hqp_responds_to_getinfo() {
        let server = MockHqpServer::start().await;

        let mut stream = TcpStream::connect(server.addr()).await.unwrap();
        stream
            .write_all(b"<?xml version=\"1.0\"?>\n")
            .await
            .unwrap();
        stream.write_all(b"<GetInfo/>\n").await.unwrap();

        let mut response = vec![0u8; 1024];
        let n = stream.read(&mut response).await.unwrap();
        let response = String::from_utf8_lossy(&response[..n]);

        assert!(response.contains("MockHQPlayer"));
        assert!(response.contains("version=\"5.0.0\""));

        server.stop().await;
    }
}
