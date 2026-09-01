//! Local browser bridge to the packaged `uhc-hiphi-pair` ceremony.
//!
//! The helper remains the only code that reads the installation signing key.
//! This module invokes that exact sibling binary without a shell, accepts only
//! the bounded public handoff contract, and projects only public ceremony data
//! back to the Settings UI.

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tokio::process::Command;

const MAX_HANDOFF_BYTES: usize = 4 * 1024;
const MAX_HELPER_OUTPUT_BYTES: usize = 16 * 1024;
const HIPHI_CLOUD_ORIGIN: &str = "https://relay.hiphi.audio";

#[derive(Debug, Serialize)]
struct PairingError {
    code: &'static str,
    message: String,
}

fn error(
    status: StatusCode,
    code: &'static str,
    message: impl Into<String>,
) -> axum::response::Response {
    (
        status,
        Json(PairingError {
            code,
            message: message.into(),
        }),
    )
        .into_response()
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EnrollmentHandoffUpload {
    pub enrollment_capability: String,
    pub installation_audience: String,
    pub expires_at: i64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InitiateRequest {
    pub enrollment: EnrollmentHandoffUpload,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompleteRequest {
    pub account_id: String,
}

#[derive(Debug, Serialize)]
pub struct PrepareResponse {
    pub installation_public_key: String,
    pub installation_fingerprint: String,
}

#[derive(Debug, Serialize)]
pub struct InitiateResponse {
    pub pairing_id: String,
    pub installation_id: String,
    pub pairing_secret: String,
    pub installation_fingerprint: String,
    pub expires_at_ms: i64,
}

#[derive(Debug, Serialize)]
pub struct CompleteResponse {
    pub paired: bool,
    pub installation_id: String,
    pub relay_endpoint: String,
    pub restart_required: bool,
}

#[derive(Debug, Serialize)]
pub struct PairingStatusResponse {
    pub paired: bool,
    pub installation_id: Option<String>,
    pub connector_state: &'static str,
}

fn helper_path() -> anyhow::Result<PathBuf> {
    let current = std::env::current_exe()?;
    for helper in helper_candidates(&current) {
        if helper.is_file() {
            return Ok(helper);
        }
    }
    anyhow::bail!(
        "the HiPhi pairing helper is missing beside the UHC executable; install a current UHC package"
    );
}

fn helper_candidates(current: &Path) -> Vec<PathBuf> {
    let mut candidates = vec![current.with_file_name(if cfg!(windows) {
        "uhc-hiphi-pair.exe"
    } else {
        "uhc-hiphi-pair"
    })];
    let filename = current
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let direct_name = match filename {
        "unified-hifi-linux-x64" => Some("uhc-hiphi-pair-x64"),
        "unified-hifi-linux-arm64" => Some("uhc-hiphi-pair-arm64"),
        "unified-hifi-linux-armv7" => Some("uhc-hiphi-pair-armv7"),
        "unified-hifi-macos-universal" => Some("uhc-hiphi-pair-macos-universal"),
        "unified-hifi-win64.exe" => Some("uhc-hiphi-pair-win64.exe"),
        _ => None,
    };
    if let Some(name) = direct_name {
        candidates.push(current.with_file_name(name));
    }
    candidates
}

async fn run_helper(arguments: &[&str]) -> anyhow::Result<BTreeMap<String, String>> {
    let output = Command::new(helper_path()?)
        .args(arguments)
        .output()
        .await?;
    if output.stdout.len() > MAX_HELPER_OUTPUT_BYTES
        || output.stderr.len() > MAX_HELPER_OUTPUT_BYTES
    {
        anyhow::bail!("the pairing helper returned an oversized response");
    }
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
        anyhow::bail!(
            "{}",
            if message.is_empty() {
                "the pairing helper refused the request"
            } else {
                message.as_str()
            }
        );
    }
    parse_helper_output(&output.stdout)
}

fn parse_helper_output(output: &[u8]) -> anyhow::Result<BTreeMap<String, String>> {
    let text = std::str::from_utf8(output)?;
    let mut fields = BTreeMap::new();
    for line in text.lines() {
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("the pairing helper returned an invalid response"))?;
        if key.is_empty()
            || value.is_empty()
            || fields.insert(key.to_string(), value.to_string()).is_some()
        {
            anyhow::bail!("the pairing helper returned an invalid response");
        }
    }
    Ok(fields)
}

fn field<'a>(fields: &'a BTreeMap<String, String>, name: &str) -> anyhow::Result<&'a str> {
    fields
        .get(name)
        .map(String::as_str)
        .ok_or_else(|| anyhow::anyhow!("the pairing helper omitted {name}"))
}

fn write_handoff(handoff: &EnrollmentHandoffUpload) -> anyhow::Result<PathBuf> {
    use std::io::Write as _;

    let bytes = serde_json::to_vec_pretty(handoff)?;
    if bytes.len() > MAX_HANDOFF_BYTES {
        anyhow::bail!("the enrollment handoff exceeds the size limit");
    }
    let directory = crate::config::get_config_dir();
    std::fs::create_dir_all(&directory)?;
    let path = directory.join(format!("hiphi-enrollment-{}.json", uuid::Uuid::new_v4()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        // QNAP shared-volume ACL inheritance can add group/other mode bits
        // after create(0600). Clear them on the open file descriptor and
        // verify the filesystem actually honored the owner-only boundary
        // before placing a one-use capability in the file.
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        if file.metadata()?.permissions().mode() & 0o077 != 0 {
            anyhow::bail!("the UHC config volume cannot enforce owner-only enrollment files");
        }
    }
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(path)
}

fn cleanup_handoff(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "failed to remove enrollment handoff")
        }
    }
}

pub async fn prepare() -> axum::response::Response {
    match run_helper(&["prepare"]).await.and_then(|fields| {
        Ok(PrepareResponse {
            installation_public_key: field(&fields, "installation_public_key")?.to_string(),
            installation_fingerprint: field(&fields, "installation_fingerprint")?.to_string(),
        })
    }) {
        Ok(response) => Json(response).into_response(),
        Err(error_value) => error(
            StatusCode::BAD_REQUEST,
            "pairing_prepare_failed",
            error_value.to_string(),
        ),
    }
}

pub async fn initiate(Json(request): Json<InitiateRequest>) -> axum::response::Response {
    let handoff_path = match write_handoff(&request.enrollment) {
        Ok(path) => path,
        Err(error_value) => {
            return error(
                StatusCode::BAD_REQUEST,
                "enrollment_invalid",
                error_value.to_string(),
            );
        }
    };
    let handoff = handoff_path.to_string_lossy().into_owned();
    let result = run_helper(&["initiate", HIPHI_CLOUD_ORIGIN, &handoff]).await;
    cleanup_handoff(&handoff_path);
    match result.and_then(|fields| {
        Ok(InitiateResponse {
            pairing_id: field(&fields, "pairing_id")?.to_string(),
            installation_id: field(&fields, "installation_id")?.to_string(),
            pairing_secret: field(&fields, "pairing_secret")?.to_string(),
            installation_fingerprint: field(&fields, "installation_fingerprint")?.to_string(),
            expires_at_ms: field(&fields, "expires_at_ms")?.parse()?,
        })
    }) {
        Ok(response) => Json(response).into_response(),
        Err(error_value) => error(
            StatusCode::BAD_REQUEST,
            "pairing_initiate_failed",
            error_value.to_string(),
        ),
    }
}

pub async fn status(State(state): State<crate::api::AppState>) -> axum::response::Response {
    match state
        .hiphi_connector
        .status_from_runtime(crate::config::get_config_dir())
        .await
    {
        Ok(status) => Json(PairingStatusResponse {
            paired: status.configured,
            installation_id: status.installation_id,
            connector_state: status.phase.as_str(),
        })
        .into_response(),
        Err(error_value) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "pairing_status_failed",
            error_value.to_string(),
        ),
    }
}

pub async fn complete(
    State(state): State<crate::api::AppState>,
    Json(request): Json<CompleteRequest>,
) -> axum::response::Response {
    let parsed = run_helper(&["complete", &request.account_id])
        .await
        .and_then(|fields| {
            if field(&fields, "pairing_complete")? != "true" {
                anyhow::bail!("the pairing helper did not report semantic success");
            }
            Ok(CompleteResponse {
                paired: true,
                installation_id: field(&fields, "UHC_HIPHI_INSTALLATION_ID")?.to_string(),
                relay_endpoint: field(&fields, "UHC_HIPHI_RELAY_URL")?.to_string(),
                restart_required: field(&fields, "connector_restart_required")? == "true",
            })
        });
    match parsed {
        Ok(mut response) => {
            match state
                .hiphi_connector
                .start_from_runtime(state.clone(), crate::config::get_config_dir())
                .await
            {
                Ok(crate::cloud_connector::runtime::ConnectorStart::Started)
                | Ok(crate::cloud_connector::runtime::ConnectorStart::AlreadyRunning) => {
                    response.restart_required = false;
                }
                Ok(crate::cloud_connector::runtime::ConnectorStart::NotConfigured) => {
                    tracing::error!("HiPhi pairing completed without persisted connector config");
                }
                Err(error_value) => {
                    tracing::error!(
                        "HiPhi pairing completed but connector activation failed: {error_value}"
                    );
                }
            }
            Json(response).into_response()
        }
        Err(error_value) => error(
            StatusCode::BAD_REQUEST,
            "pairing_complete_failed",
            error_value.to_string(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_output_rejects_duplicates_and_malformed_lines() {
        assert!(parse_helper_output(b"a=one\na=two\n").is_err());
        assert!(parse_helper_output(b"not-a-field\n").is_err());
        assert!(parse_helper_output(b"=value\n").is_err());
        assert!(parse_helper_output(b"key=\n").is_err());
    }

    #[test]
    fn helper_output_preserves_equals_inside_values() {
        let fields = parse_helper_output(b"key=value=with=equals\n").unwrap();
        assert_eq!(
            fields.get("key").map(String::as_str),
            Some("value=with=equals")
        );
    }

    #[test]
    fn packaged_and_direct_binary_names_find_the_matching_pairing_helper() {
        let packaged = helper_candidates(Path::new("/opt/uhc/unified-hifi-control"));
        assert_eq!(
            packaged[0].file_name().and_then(|name| name.to_str()),
            Some(if cfg!(windows) {
                "uhc-hiphi-pair.exe"
            } else {
                "uhc-hiphi-pair"
            })
        );

        for (server, helper) in [
            ("unified-hifi-linux-x64", "uhc-hiphi-pair-x64"),
            ("unified-hifi-linux-arm64", "uhc-hiphi-pair-arm64"),
            ("unified-hifi-linux-armv7", "uhc-hiphi-pair-armv7"),
            (
                "unified-hifi-macos-universal",
                "uhc-hiphi-pair-macos-universal",
            ),
            ("unified-hifi-win64.exe", "uhc-hiphi-pair-win64.exe"),
        ] {
            assert!(helper_candidates(Path::new(server))
                .iter()
                .any(|path| { path.file_name().and_then(|name| name.to_str()) == Some(helper) }));
        }
    }
}
