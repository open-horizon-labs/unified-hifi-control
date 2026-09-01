//! Local half of the HiPhi installation-pairing ceremony.
//!
//! `prepare` creates the installation key without contacting the cloud. A
//! signed-in owner uses its public key to mint a one-use enrollment handoff.
//! `initiate` consumes that owner-only handoff and asks the cloud for a
//! short-lived ceremony. The signed-in owner must independently claim the
//! one-time secret and confirm the displayed fingerprint. `complete` then
//! proves possession of the local key and consumes the ceremony. No owner
//! bearer or cloud credential is accepted or written by this command.

#[cfg(feature = "server")]
mod command {
    use std::{fs, io::Write as _, path::Path};

    use anyhow::Context as _;
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    use serde::de::DeserializeOwned;
    use serde::{Deserialize, Serialize};
    use unified_hifi_control::cloud_connector::{
        pairing::{enrollment_possession_message, possession_message},
        CloudConnectorConfig, InstallationIdentity,
    };
    use url::Url;
    use uuid::Uuid;

    const INSTALLATION_ENROLLMENT_AUDIENCE: &str = "uhc-connector";
    const MAX_ENROLLMENT_TTL_MS: i64 = 10 * 60 * 1_000;
    const MAX_PAIRING_TTL_MS: i64 = 10 * 60 * 1_000;
    const MAX_HANDOFF_BYTES: u64 = 4 * 1_024;
    const MAX_JSON_RESPONSE_BYTES: usize = 16 * 1_024;

    #[derive(Clone, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct EnrollmentHandoff {
        enrollment_capability: String,
        installation_audience: String,
        expires_at: i64,
    }

    #[derive(Clone, Deserialize)]
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
        session_issuer_keys: serde_json::Value,
        command_issuer_keys: serde_json::Value,
    }

    pub async fn run() -> anyhow::Result<()> {
        let mut arguments = std::env::args().skip(1);
        match arguments.next().as_deref() {
            Some("prepare") => {
                if arguments.next().is_some() {
                    anyhow::bail!("prepare accepts no arguments");
                }
                prepare()
            }
            Some("initiate") => {
                let origin = arguments
                    .next()
                    .context("usage: uhc-hiphi-pair initiate https://cloud.example /owner-only/enrollment.json")?;
                let handoff_path = arguments
                    .next()
                    .context("usage: uhc-hiphi-pair initiate https://cloud.example /owner-only/enrollment.json")?;
                if arguments.next().is_some() {
                    anyhow::bail!("initiate accepts exactly one cloud origin and enrollment handoff path");
                }
                initiate(&origin, Path::new(&handoff_path)).await
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
                "usage: uhc-hiphi-pair prepare\n       uhc-hiphi-pair initiate https://cloud.example /owner-only/enrollment.json\n       uhc-hiphi-pair complete <account-uuid>"
            ),
        }
    }

    fn prepare() -> anyhow::Result<()> {
        let config_dir = unified_hifi_control::config::get_config_dir();
        fs::create_dir_all(&config_dir)?;
        let key_path = config_dir.join("hiphi-installation.key");
        if CloudConnectorConfig::from_runtime(&config_dir)?.is_some() {
            anyhow::bail!("this UHC is already configured with a HiPhi installation identity");
        }
        let identity = load_or_create_identity(&key_path)?;
        println!(
            "installation_public_key={}",
            URL_SAFE_NO_PAD.encode(identity.verifying_key().as_bytes())
        );
        println!("installation_fingerprint={}", identity.fingerprint());
        println!("installation_key_path={}", key_path.display());
        println!("owner_authorized_enrollment_required=true");
        println!("owner_bearer_must_not_be_copied_to_uhc=true");
        Ok(())
    }

    async fn initiate(origin: &str, handoff_path: &Path) -> anyhow::Result<()> {
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
        if CloudConnectorConfig::from_runtime(&config_dir)?.is_some() {
            anyhow::bail!("this UHC is already configured with a HiPhi installation identity");
        }
        let identity =
            InstallationIdentity::load(&key_path, format!("pending_{}", Uuid::new_v4().simple()))
                .context(
                "no prepared installation key is available; run `uhc-hiphi-pair prepare` first",
            )?;
        let handoff = read_enrollment_handoff(handoff_path)?;
        validate_enrollment(&handoff, unix_ms())?;
        let proof = identity.sign(&enrollment_possession_message(
            &handoff.enrollment_capability,
            &handoff.installation_audience,
            &identity.fingerprint(),
        ));
        let initiate_url = endpoint(&origin, "/v1/pairing/initiate")?;
        let response: InitiationResponse = send_json(
            &client()?,
            &initiate_url,
            &serde_json::json!({
                "installation_public_key": URL_SAFE_NO_PAD.encode(identity.verifying_key().as_bytes()),
                "enrollment_capability": handoff.enrollment_capability,
                "installation_audience": handoff.installation_audience,
                "possession_signature": URL_SAFE_NO_PAD.encode(proof.to_bytes()),
            }),
        )
        .await?;
        validate_initiation(&response, &identity.fingerprint(), unix_ms())?;
        let pending = PendingPairing {
            cloud_origin: origin.to_string(),
            pairing_id: response.pairing_id,
            installation_id: response.installation_id,
            secret: response.secret,
            fingerprint: response.fingerprint,
            expires_at: response.expires_at,
        };
        write_owner_only(&pending_path, &serde_json::to_vec_pretty(&pending)?)?;
        ensure_owner_only_regular(handoff_path)?;
        fs::remove_file(handoff_path)
            .context("pairing began, but the consumed enrollment handoff could not be removed")?;

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
        let client = client()?;
        let confirm_url = endpoint(&cloud_origin, "/v1/pairing/confirm-local")?;
        let confirmation: SemanticSuccess = send_json(
            &client,
            &confirm_url,
            &serde_json::json!({
                "pairing_id": pending.pairing_id,
                "secret": pending.secret,
                "account_id": account_id,
                "fingerprint": pending.fingerprint,
                "possession_signature": URL_SAFE_NO_PAD.encode(proof.to_bytes()),
            }),
        )
        .await?;
        if !confirmation.ok {
            anyhow::bail!("cloud did not confirm local possession");
        }
        let completion_url = endpoint(&cloud_origin, "/v1/pairing/complete")?;
        let response: CompletionResponse = send_json(
            &client,
            &completion_url,
            &serde_json::json!({
                "pairing_id": pending.pairing_id,
                "secret": pending.secret,
                "account_id": account_id,
            }),
        )
        .await?;
        if response.installation_id != pending.installation_id {
            anyhow::bail!("cloud completed a different installation");
        }
        let session_keys = serde_json::to_string(&response.session_issuer_keys)?;
        let command_keys = serde_json::to_string(&response.command_issuer_keys)?;
        let connector = unified_hifi_control::cloud_connector::CloudConnectorConfig::from_values(
            config_dir.clone(),
            &response.relay_endpoint,
            &response.installation_id.to_string(),
            &session_keys,
            &command_keys,
        )?;
        let environment_path = config_dir.join("hiphi.env");
        persist_connector_environment(
            &environment_path,
            connector.endpoint.as_str(),
            &response.installation_id.to_string(),
            &session_keys,
            &command_keys,
        )?;
        fs::remove_file(&pending_path)?;
        println!("pairing_complete=true");
        println!("UHC_HIPHI_INSTALLATION_ID={}", response.installation_id);
        println!("UHC_HIPHI_RELAY_URL={}", connector.endpoint.as_str());
        println!("connector_environment_path={}", environment_path.display());
        println!("connector_restart_required=true");
        Ok(())
    }

    fn validated_origin(input: &str) -> anyhow::Result<Url> {
        let url = Url::parse(input)?;
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.port_or_known_default() != Some(443)
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
        {
            anyhow::bail!("cloud origin must be an exact credential-free HTTPS origin on port 443");
        }
        Ok(url)
    }

    fn endpoint(origin: &Url, path: &str) -> anyhow::Result<Url> {
        let mut url = origin.clone();
        url.set_path(path);
        Ok(url)
    }

    fn client() -> anyhow::Result<reqwest::Client> {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .context("failed to build pairing HTTP client")
    }

    async fn send_json<T: Serialize + ?Sized, R: DeserializeOwned>(
        client: &reqwest::Client,
        endpoint: &Url,
        request: &T,
    ) -> anyhow::Result<R> {
        let mut response = client.post(endpoint.clone()).json(request).send().await?;
        if response.url() != endpoint {
            anyhow::bail!("cloud response origin or route did not match the requested endpoint");
        }
        let status = response.status();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let bytes = read_bounded(&mut response, MAX_JSON_RESPONSE_BYTES).await?;
        if !status.is_success() {
            anyhow::bail!("cloud request failed with HTTP {}", status.as_u16());
        }
        if !content_type
            .split(';')
            .next()
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
        {
            anyhow::bail!("cloud returned a non-JSON response");
        }
        serde_json::from_slice(&bytes).context("cloud returned an invalid response contract")
    }

    async fn read_bounded(
        response: &mut reqwest::Response,
        limit: usize,
    ) -> anyhow::Result<Vec<u8>> {
        if response
            .content_length()
            .is_some_and(|length| length > limit as u64)
        {
            anyhow::bail!("cloud response exceeded the size limit");
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if bytes.len().saturating_add(chunk.len()) > limit {
                anyhow::bail!("cloud response exceeded the size limit");
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }

    fn load_or_create_identity(key_path: &Path) -> anyhow::Result<InstallationIdentity> {
        let placeholder = format!("pending_{}", Uuid::new_v4().simple());
        if key_path.exists() {
            return InstallationIdentity::load(key_path, placeholder).map_err(Into::into);
        }
        let identity = InstallationIdentity::generate(placeholder)?;
        identity.save(key_path)?;
        Ok(identity)
    }

    fn read_enrollment_handoff(path: &Path) -> anyhow::Result<EnrollmentHandoff> {
        ensure_owner_only_regular(path)?;
        let metadata = fs::symlink_metadata(path)?;
        if metadata.len() > MAX_HANDOFF_BYTES {
            anyhow::bail!("enrollment handoff exceeds the size limit");
        }
        let bytes = fs::read(path)?;
        if bytes.len() as u64 > MAX_HANDOFF_BYTES {
            anyhow::bail!("enrollment handoff exceeds the size limit");
        }
        serde_json::from_slice(&bytes).context("enrollment handoff has an invalid contract")
    }

    fn ensure_owner_only_regular(path: &Path) -> anyhow::Result<()> {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.file_type().is_file() {
            anyhow::bail!("enrollment handoff must be a regular, non-symlink file");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            if metadata.permissions().mode() & 0o077 != 0 {
                anyhow::bail!("enrollment handoff must be owner-only");
            }
        }
        Ok(())
    }

    fn validate_enrollment(handoff: &EnrollmentHandoff, now: i64) -> anyhow::Result<()> {
        let capability = URL_SAFE_NO_PAD
            .decode(&handoff.enrollment_capability)
            .context("enrollment capability is not canonical base64url")?;
        if capability.len() != 32
            || URL_SAFE_NO_PAD.encode(&capability) != handoff.enrollment_capability
        {
            anyhow::bail!("enrollment capability is not a canonical 32-byte value");
        }
        if handoff.installation_audience != INSTALLATION_ENROLLMENT_AUDIENCE {
            anyhow::bail!("enrollment audience is not the UHC connector audience");
        }
        if handoff.expires_at <= now
            || handoff.expires_at.saturating_sub(now) > MAX_ENROLLMENT_TTL_MS
        {
            anyhow::bail!("enrollment handoff is expired or unreasonably long-lived");
        }
        Ok(())
    }

    fn validate_initiation(
        response: &InitiationResponse,
        expected_fingerprint: &str,
        now: i64,
    ) -> anyhow::Result<()> {
        if response.fingerprint != expected_fingerprint {
            anyhow::bail!("cloud returned a fingerprint that does not match the local key");
        }
        let secret = URL_SAFE_NO_PAD
            .decode(&response.secret)
            .context("cloud returned a noncanonical pairing secret")?;
        if secret.len() != 32 || URL_SAFE_NO_PAD.encode(&secret) != response.secret {
            anyhow::bail!("cloud returned an invalid pairing secret");
        }
        if response.expires_at <= now
            || response.expires_at.saturating_sub(now) > MAX_PAIRING_TTL_MS
        {
            anyhow::bail!("cloud returned an expired or unreasonably long pairing ceremony");
        }
        Ok(())
    }

    fn unix_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct SemanticSuccess {
        ok: bool,
    }

    #[cfg(unix)]
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

    #[cfg(not(unix))]
    fn write_owner_only(_path: &Path, _bytes: &[u8]) -> anyhow::Result<()> {
        anyhow::bail!("HiPhi enrollment requires verified owner-only file permissions")
    }

    #[cfg(unix)]
    fn persist_connector_environment(
        path: &Path,
        relay_endpoint: &str,
        installation_id: &str,
        session_keys: &str,
        command_keys: &str,
    ) -> anyhow::Result<()> {
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

        let parent = path
            .parent()
            .context("connector environment has no parent")?;
        fs::create_dir_all(parent)?;
        let temporary = parent.join(format!(".hiphi.env.{}.tmp", Uuid::new_v4().simple()));
        let bytes = format!(
            "UHC_HIPHI_RELAY_URL={relay_endpoint}\nUHC_HIPHI_INSTALLATION_ID={installation_id}\nUHC_HIPHI_SESSION_ISSUER_KEYS={session_keys}\nUHC_HIPHI_COMMAND_ISSUER_KEYS={command_keys}\n"
        );
        let write_result = (|| -> anyhow::Result<()> {
            let mut file = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(&temporary)?;
            file.write_all(bytes.as_bytes())?;
            file.sync_all()?;
            fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
            fs::rename(&temporary, path)?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
            fs::File::open(parent)?.sync_all()?;
            Ok(())
        })();
        if write_result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        write_result
    }

    #[cfg(not(unix))]
    fn persist_connector_environment(
        _path: &Path,
        _relay_endpoint: &str,
        _installation_id: &str,
        _session_keys: &str,
        _command_keys: &str,
    ) -> anyhow::Result<()> {
        anyhow::bail!("HiPhi enrollment requires verified owner-only file permissions")
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn cloud_origin_is_an_exact_default_port_https_origin() {
            assert!(validated_origin("https://cloud.example").is_ok());
            for invalid in [
                "http://cloud.example",
                "https://cloud.example:8443",
                "https://user@cloud.example",
                "https://cloud.example/a/path",
                "https://cloud.example?query=yes",
                "https://cloud.example/#fragment",
            ] {
                assert!(validated_origin(invalid).is_err(), "accepted {invalid}");
            }
        }

        #[test]
        fn enrollment_handoff_is_canonical_bounded_and_short_lived() {
            let now = 1_000_000_i64;
            let capability = URL_SAFE_NO_PAD.encode([7_u8; 32]);
            let valid = EnrollmentHandoff {
                enrollment_capability: capability.clone(),
                installation_audience: INSTALLATION_ENROLLMENT_AUDIENCE.to_owned(),
                expires_at: now + 60_000,
            };
            assert!(validate_enrollment(&valid, now).is_ok());

            for invalid in [
                EnrollmentHandoff {
                    enrollment_capability: format!("{capability}="),
                    ..valid.clone()
                },
                EnrollmentHandoff {
                    installation_audience: "owner-api".to_owned(),
                    ..valid.clone()
                },
                EnrollmentHandoff {
                    expires_at: now,
                    ..valid.clone()
                },
                EnrollmentHandoff {
                    expires_at: now + MAX_ENROLLMENT_TTL_MS + 1,
                    ..valid.clone()
                },
            ] {
                assert!(validate_enrollment(&invalid, now).is_err());
            }
        }

        #[test]
        fn prepared_key_and_enrollment_handoff_are_owner_only_regular_files() {
            use std::os::unix::fs::{symlink, PermissionsExt as _};

            let directory = tempfile::tempdir().unwrap();
            let key_path = directory.path().join("installation.key");
            load_or_create_identity(&key_path).unwrap();
            assert_eq!(
                fs::symlink_metadata(&key_path)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );

            let handoff_path = directory.path().join("enrollment.json");
            let handoff = serde_json::json!({
                "enrollment_capability": URL_SAFE_NO_PAD.encode([7_u8; 32]),
                "installation_audience": INSTALLATION_ENROLLMENT_AUDIENCE,
                "expires_at": unix_ms() + 60_000,
            });
            write_owner_only(&handoff_path, &serde_json::to_vec(&handoff).unwrap()).unwrap();
            assert!(read_enrollment_handoff(&handoff_path).is_ok());

            fs::set_permissions(&handoff_path, fs::Permissions::from_mode(0o644)).unwrap();
            assert!(read_enrollment_handoff(&handoff_path).is_err());
            fs::set_permissions(&handoff_path, fs::Permissions::from_mode(0o600)).unwrap();
            let symlink_path = directory.path().join("enrollment-link.json");
            symlink(&handoff_path, &symlink_path).unwrap();
            assert!(read_enrollment_handoff(&symlink_path).is_err());
        }

        #[test]
        fn cloud_response_contracts_reject_unknown_or_missing_authority_fields() {
            let valid = serde_json::json!({
                "pairing_id": Uuid::from_u128(1),
                "installation_id": Uuid::from_u128(2),
                "secret": "not-a-real-pairing-secret-value",
                "fingerprint": "AAAA-BBBB",
                "expires_at": unix_ms() + 60_000,
            });
            assert!(serde_json::from_value::<InitiationResponse>(valid.clone()).is_ok());

            let mut unknown = valid.clone();
            unknown["owner_bearer"] = serde_json::json!("must-never-enter-uhc");
            assert!(serde_json::from_value::<InitiationResponse>(unknown).is_err());
            let mut missing = valid;
            missing.as_object_mut().unwrap().remove("fingerprint");
            assert!(serde_json::from_value::<InitiationResponse>(missing).is_err());

            assert!(
                serde_json::from_value::<SemanticSuccess>(serde_json::json!({"ok": true})).is_ok()
            );
            assert!(serde_json::from_value::<SemanticSuccess>(
                serde_json::json!({"ok": true, "authority": "extra"})
            )
            .is_err());

            let completion = serde_json::json!({
                "installation_id": Uuid::from_u128(2),
                "relay_endpoint": "wss://cloud.example/v1/relay/connect",
                "session_issuer_keys": {
                    "version": 1,
                    "keys": [{"kid": "session-v1", "key": URL_SAFE_NO_PAD.encode([3_u8; 32])}],
                },
                "command_issuer_keys": {
                    "version": 1,
                    "keys": [{"kid": "command-v1", "key": URL_SAFE_NO_PAD.encode([4_u8; 32])}],
                },
            });
            assert!(serde_json::from_value::<CompletionResponse>(completion.clone()).is_ok());
            let mut incomplete = completion;
            incomplete
                .as_object_mut()
                .unwrap()
                .remove("command_issuer_keys");
            assert!(serde_json::from_value::<CompletionResponse>(incomplete).is_err());
        }

        #[test]
        fn completed_pairing_environment_is_atomic_private_and_complete() {
            use std::os::unix::fs::PermissionsExt as _;

            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("hiphi.env");
            persist_connector_environment(
                &path,
                "wss://cloud.example/v1/relay/connect",
                "11111111-1111-4111-8111-111111111111",
                r#"{"version":1,"keys":[{"kid":"session-v1","key":"session-public-key"}]}"#,
                r#"{"version":1,"keys":[{"kid":"command-v1","key":"command-public-key"}]}"#,
            )
            .unwrap();

            let content = fs::read_to_string(&path).unwrap();
            for key in [
                "UHC_HIPHI_RELAY_URL=",
                "UHC_HIPHI_INSTALLATION_ID=",
                "UHC_HIPHI_SESSION_ISSUER_KEYS=",
                "UHC_HIPHI_COMMAND_ISSUER_KEYS=",
            ] {
                assert!(content.contains(key));
            }
            assert_eq!(content.lines().count(), 4);
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert!(fs::read_dir(directory.path()).unwrap().all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".tmp")
            }));
        }

        #[test]
        fn pairing_ceremony_has_its_own_short_bounded_expiry_window() {
            let now = 1_000_000_i64;
            let response = InitiationResponse {
                pairing_id: Uuid::from_u128(1),
                installation_id: Uuid::from_u128(2),
                secret: URL_SAFE_NO_PAD.encode([9_u8; 32]),
                fingerprint: "AAAA-BBBB".to_owned(),
                expires_at: now + 5 * 60 * 1_000,
            };
            assert!(validate_initiation(&response, "AAAA-BBBB", now).is_ok());

            for invalid in [
                InitiationResponse {
                    fingerprint: "CCCC-DDDD".to_owned(),
                    ..response.clone()
                },
                InitiationResponse {
                    expires_at: now,
                    ..response.clone()
                },
                InitiationResponse {
                    expires_at: now + MAX_PAIRING_TTL_MS + 1,
                    ..response
                },
            ] {
                assert!(validate_initiation(&invalid, "AAAA-BBBB", now).is_err());
            }
        }
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
