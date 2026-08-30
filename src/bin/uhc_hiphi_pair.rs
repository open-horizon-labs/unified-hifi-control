//! Local half of the HiPhi installation-pairing ceremony.
//!
//! `initiate` creates the installation key and asks the cloud for a short-lived
//! ceremony. The signed-in owner must independently claim the one-time secret
//! and confirm the displayed fingerprint. `complete` then proves possession of
//! the local key and consumes the ceremony. No cloud credential is written by
//! this command.

#[cfg(feature = "server")]
mod command {
    use std::{fs, io::Write as _, path::Path};

    use anyhow::Context as _;
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    use serde::{Deserialize, Serialize};
    use unified_hifi_control::cloud_connector::{
        pairing::possession_message, InstallationIdentity,
    };
    use url::Url;
    use uuid::Uuid;

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct InitiationResponse {
        pairing_id: Uuid,
        installation_id: Uuid,
        secret: String,
        fingerprint: String,
        expires_at: i64,
    }

    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct PendingPairing {
        cloud_origin: String,
        pairing_id: Uuid,
        installation_id: Uuid,
        secret: String,
        fingerprint: String,
        expires_at: i64,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct CompletionResponse {
        installation_id: Uuid,
        relay_endpoint: String,
    }

    pub async fn run() -> anyhow::Result<()> {
        let mut arguments = std::env::args().skip(1);
        match arguments.next().as_deref() {
            Some("initiate") => {
                let origin = arguments
                    .next()
                    .context("usage: uhc-hiphi-pair initiate https://cloud.example")?;
                if arguments.next().is_some() {
                    anyhow::bail!("initiate accepts exactly one cloud origin");
                }
                initiate(&origin).await
            }
            Some("complete") => {
                let account_id = arguments
                    .next()
                    .context("usage: uhc-hiphi-pair complete <account-uuid>")?;
                if arguments.next().is_some() {
                    anyhow::bail!("complete accepts exactly one account id");
                }
                complete(&account_id).await
            }
            _ => anyhow::bail!(
                "usage: uhc-hiphi-pair initiate https://cloud.example\n       uhc-hiphi-pair complete <account-uuid>"
            ),
        }
    }

    async fn initiate(origin: &str) -> anyhow::Result<()> {
        let origin = validated_origin(origin)?;
        let config_dir = unified_hifi_control::config::get_config_dir();
        fs::create_dir_all(&config_dir)?;
        let key_path = config_dir.join("hiphi-installation.key");
        let pending_path = config_dir.join("hiphi-pairing.json");
        if pending_path.exists() {
            anyhow::bail!(
                "a pairing ceremony is already pending at {}; complete or remove it first",
                pending_path.display()
            );
        }
        if key_path.exists() && std::env::var_os("UHC_HIPHI_INSTALLATION_ID").is_some() {
            anyhow::bail!("this UHC is already configured with a HiPhi installation identity");
        }
        let placeholder = format!("pending_{}", Uuid::new_v4().simple());
        let identity = if key_path.exists() {
            InstallationIdentity::load(&key_path, placeholder)?
        } else {
            let identity = InstallationIdentity::generate(placeholder)?;
            identity.save(&key_path)?;
            identity
        };
        let response = client()?
            .post(endpoint(&origin, "/v1/pairing/initiate")?)
            .json(&serde_json::json!({
                "installation_public_key": URL_SAFE_NO_PAD.encode(identity.verifying_key().as_bytes()),
            }))
            .send()
            .await?
            .error_for_status()?
            .json::<InitiationResponse>()
            .await?;
        if response.fingerprint != identity.fingerprint() {
            anyhow::bail!("cloud returned a fingerprint that does not match the local key");
        }
        if response.secret.len() < 32 || response.expires_at <= unix_ms() {
            anyhow::bail!("cloud returned an invalid or expired pairing ceremony");
        }
        let pending = PendingPairing {
            cloud_origin: origin.to_string(),
            pairing_id: response.pairing_id,
            installation_id: response.installation_id,
            secret: response.secret,
            fingerprint: response.fingerprint,
            expires_at: response.expires_at,
        };
        write_owner_only(&pending_path, &serde_json::to_vec_pretty(&pending)?)?;

        println!("pairing_id={}", pending.pairing_id);
        println!("installation_id={}", pending.installation_id);
        println!("pairing_secret={}", pending.secret);
        println!("installation_fingerprint={}", pending.fingerprint);
        println!("expires_at_ms={}", pending.expires_at);
        println!("owner_claim_and_fingerprint_confirmation_required=true");
        println!("pending_state_path={}", pending_path.display());
        Ok(())
    }

    async fn complete(account_id: &str) -> anyhow::Result<()> {
        let account_id = Uuid::parse_str(account_id).context("account id is not a UUID")?;
        let config_dir = unified_hifi_control::config::get_config_dir();
        let key_path = config_dir.join("hiphi-installation.key");
        let pending_path = config_dir.join("hiphi-pairing.json");
        let pending: PendingPairing = serde_json::from_slice(
            &fs::read(&pending_path).context("no locally initiated pairing is pending")?,
        )?;
        let cloud_origin = validated_origin(&pending.cloud_origin)?;
        if pending.expires_at <= unix_ms() {
            anyhow::bail!("the pairing ceremony expired; initiate a new one");
        }
        let identity = InstallationIdentity::load(&key_path, pending.installation_id.to_string())?;
        if pending.fingerprint != identity.fingerprint() {
            anyhow::bail!("pending pairing fingerprint no longer matches the local key");
        }
        let proof = identity.sign(&possession_message(
            pending.pairing_id,
            pending.installation_id,
            account_id,
            &pending.fingerprint,
        ));
        client()?
            .post(endpoint(&cloud_origin, "/v1/pairing/confirm-local")?)
            .json(&serde_json::json!({
                "pairing_id": pending.pairing_id,
                "secret": pending.secret,
                "account_id": account_id,
                "fingerprint": pending.fingerprint,
                "possession_signature": URL_SAFE_NO_PAD.encode(proof.to_bytes()),
            }))
            .send()
            .await?
            .error_for_status()?;
        let response = client()?
            .post(endpoint(&cloud_origin, "/v1/pairing/complete")?)
            .json(&serde_json::json!({
                "pairing_id": pending.pairing_id,
                "secret": pending.secret,
                "account_id": account_id,
            }))
            .send()
            .await?
            .error_for_status()?
            .json::<CompletionResponse>()
            .await?;
        if response.installation_id != pending.installation_id {
            anyhow::bail!("cloud completed a different installation");
        }
        let relay = unified_hifi_control::cloud_connector::transport::RelayEndpoint::parse(
            &response.relay_endpoint,
        )?;
        fs::remove_file(&pending_path)?;
        println!("pairing_complete=true");
        println!("UHC_HIPHI_INSTALLATION_ID={}", response.installation_id);
        println!("UHC_HIPHI_RELAY_URL={}", relay.as_str());
        println!("issuer_public_key_configuration_required=true");
        Ok(())
    }

    fn validated_origin(input: &str) -> anyhow::Result<Url> {
        let mut url = Url::parse(input)?;
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            anyhow::bail!("cloud origin must be credential-free HTTPS without query or fragment");
        }
        url.set_path("");
        Ok(url)
    }

    fn endpoint(origin: &Url, path: &str) -> anyhow::Result<Url> {
        let mut url = origin.clone();
        url.set_path(path);
        Ok(url)
    }

    fn client() -> anyhow::Result<reqwest::Client> {
        reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .context("failed to build pairing HTTP client")
    }

    fn unix_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64
    }

    fn write_owner_only(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        Ok(())
    }
}

#[cfg(feature = "server")]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    command::run().await
}

#[cfg(not(feature = "server"))]
fn main() {
    eprintln!("uhc-hiphi-pair requires the server feature");
    std::process::exit(2);
}
