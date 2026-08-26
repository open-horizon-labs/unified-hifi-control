//! Startup MQTT configuration handed to UHC by its environment (#605).
//!
//! The Home Assistant add-on knows the broker's host, port and credentials
//! before UHC ever starts: the Supervisor hands them to any add-on that
//! declares `services: ["mqtt:want"]`. Without a way to pass that through,
//! installing the add-on produced a UI panel and zero Home Assistant
//! entities, because the discovery publisher sat unconfigured with nothing
//! saying so.
//!
//! This module is the receiving end: `run.sh` exports `UHC_MQTT_*`, and the
//! server adopts them at startup under three rules that keep the user in
//! charge of their own installation.
//!
//! 1. **The user always wins.** Broker settings saved through
//!    `POST /api/mqtt/configure` are marked [`MqttConfigSource::User`] and
//!    are never overwritten, however the environment is set. Records written
//!    before provenance existed default to `User` for exactly this reason.
//! 2. **Re-applying is free.** An environment config identical to the one
//!    already stored is adopted in memory and written nowhere, so a restart
//!    loop cannot churn the encrypted credential file.
//! 3. **Enabling happens once per config.** The publisher is switched on (and
//!    that choice persisted to `app-settings.json`) only when the stored
//!    record actually changes. Once the setting is on disk, a user who turns
//!    the toggle back off stays off across restarts - later boots see an
//!    unchanged record and leave the toggle alone.
//!
//! The bootstrap decision itself is a pure function, [`plan`], so all four
//! rules are unit-testable without touching the filesystem or the environment.

use super::credentials::{MqttConfigSource, MqttCredentialRecord};
use crate::mqtt::{DEFAULT_BASE_TOPIC, DEFAULT_DISCOVERY_PREFIX, DEFAULT_PORT, DEFAULT_TLS_PORT};

pub const HOST_ENV: &str = "UHC_MQTT_HOST";
pub const PORT_ENV: &str = "UHC_MQTT_PORT";
pub const USERNAME_ENV: &str = "UHC_MQTT_USERNAME";
pub const PASSWORD_ENV: &str = "UHC_MQTT_PASSWORD";
pub const TLS_ENV: &str = "UHC_MQTT_TLS";
pub const BASE_TOPIC_ENV: &str = "UHC_MQTT_BASE_TOPIC";
pub const DISCOVERY_PREFIX_ENV: &str = "UHC_MQTT_DISCOVERY_PREFIX";

/// What startup should do with the environment's MQTT configuration, given
/// what is already stored. Returned by [`plan`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MqttBootstrap {
    /// The environment supplies nothing and nothing is stored: the publisher
    /// stays unconfigured until someone fills in the settings form.
    Unconfigured,
    /// Use the stored record as-is. The environment either supplies nothing
    /// or lost to a user-owned record; either way startup writes nothing and
    /// leaves the enable toggle entirely to `app-settings.json`.
    UseStored(MqttCredentialRecord),
    /// The stored record already *is* this environment config. Adopt it in
    /// memory, write nothing, and do not re-enable - that decision was made
    /// (and persisted) on the boot that first applied it.
    EnvironmentUnchanged(MqttCredentialRecord),
    /// Save this record and turn the publisher on, so entities appear without
    /// the user having to find the toggle.
    ApplyEnvironment(MqttCredentialRecord),
}

/// Read `UHC_MQTT_*` from the process environment.
pub fn from_env() -> Result<Option<MqttCredentialRecord>, String> {
    from_lookup(|key| std::env::var(key).ok())
}

/// Build the environment's broker record from an arbitrary lookup, so the
/// parsing rules can be tested without mutating the real (process-global,
/// test-hostile) environment.
///
/// `UHC_MQTT_HOST` is the trigger: with no host there is no environment
/// config at all, and every other variable is ignored. A host that is present
/// but unusable is an error rather than a silent `None`, because a
/// misconfigured add-on should say so in the log instead of looking like a
/// plain unconfigured install.
pub fn from_lookup(
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<Option<MqttCredentialRecord>, String> {
    let Some(host) = lookup(HOST_ENV) else {
        return Ok(None);
    };
    let host = host.trim().to_string();
    if host.is_empty() {
        return Ok(None);
    }

    let tls = match lookup(TLS_ENV) {
        Some(value) => parse_bool(&value)
            .ok_or_else(|| format!("{TLS_ENV} must be true/false, got {value:?}"))?,
        None => false,
    };
    let port = match lookup(PORT_ENV).map(|value| value.trim().to_string()) {
        Some(value) if !value.is_empty() => value
            .parse::<u16>()
            .map_err(|_| format!("{PORT_ENV} must be a port number, got {value:?}"))?,
        // The Supervisor omits the port only when it has nothing to say; the
        // MQTT defaults are the same ones the settings form applies.
        _ => {
            if tls {
                DEFAULT_TLS_PORT
            } else {
                DEFAULT_PORT
            }
        }
    };

    Ok(Some(MqttCredentialRecord {
        host,
        port,
        tls,
        username: non_empty(lookup(USERNAME_ENV)),
        // The password is deliberately not trimmed: leading/trailing
        // whitespace is legal in a broker password and trimming it would
        // silently produce a credential that cannot authenticate.
        password: lookup(PASSWORD_ENV).filter(|value| !value.is_empty()),
        base_topic: non_empty(lookup(BASE_TOPIC_ENV))
            .unwrap_or_else(|| DEFAULT_BASE_TOPIC.to_string()),
        discovery_prefix: non_empty(lookup(DISCOVERY_PREFIX_ENV))
            .unwrap_or_else(|| DEFAULT_DISCOVERY_PREFIX.to_string()),
        source: MqttConfigSource::Environment,
    }))
}

fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Accept the spellings a shell script and the Supervisor's JSON can produce.
fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" | "" => Some(false),
        _ => None,
    }
}

/// Decide what startup does, given the stored record and the environment's.
///
/// Pure on purpose: the precedence rules in this module's documentation are
/// the whole feature, and they are the part worth pinning down in tests.
pub fn plan(
    stored: Option<MqttCredentialRecord>,
    environment: Option<MqttCredentialRecord>,
) -> MqttBootstrap {
    match (stored, environment) {
        (None, None) => MqttBootstrap::Unconfigured,
        (Some(stored), None) => MqttBootstrap::UseStored(stored),
        // Rule 1: settings the user typed in are never overwritten, and the
        // environment does not get to re-enable a publisher they turned off.
        (Some(stored), Some(_)) if stored.source == MqttConfigSource::User => {
            MqttBootstrap::UseStored(stored)
        }
        // Rules 2 and 3: an unchanged environment config writes nothing and
        // re-enables nothing.
        (Some(stored), Some(environment)) if stored == environment => {
            MqttBootstrap::EnvironmentUnchanged(stored)
        }
        (_, Some(environment)) => MqttBootstrap::ApplyEnvironment(environment),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_record() -> MqttCredentialRecord {
        MqttCredentialRecord {
            host: "core-mosquitto".to_string(),
            port: 1883,
            tls: false,
            username: Some("addons".to_string()),
            password: Some("supervisor-secret".to_string()),
            base_topic: DEFAULT_BASE_TOPIC.to_string(),
            discovery_prefix: DEFAULT_DISCOVERY_PREFIX.to_string(),
            source: MqttConfigSource::Environment,
        }
    }

    fn user_record() -> MqttCredentialRecord {
        MqttCredentialRecord {
            host: "broker.lan".to_string(),
            port: 8883,
            tls: true,
            username: None,
            password: None,
            base_topic: "my-hifi".to_string(),
            discovery_prefix: DEFAULT_DISCOVERY_PREFIX.to_string(),
            source: MqttConfigSource::User,
        }
    }

    /// Stand-in for `std::env::var`, so precedence and parsing are tested
    /// without mutating the process-global environment (which would make
    /// these tests order-dependent under the default threaded test runner).
    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect();
        move |key| {
            owned
                .iter()
                .find(|(candidate, _)| candidate == key)
                .map(|(_, value)| value.clone())
        }
    }

    #[test]
    fn no_host_means_no_environment_config() {
        let parsed = from_lookup(lookup(&[(PORT_ENV, "1883")])).expect("parse");
        assert_eq!(parsed, None);
        // A host that is set but blank is the same as unset: `run.sh` exports
        // whatever the Supervisor returned, and an unprovisioned service
        // yields an empty string rather than an absent variable.
        let blank = from_lookup(lookup(&[(HOST_ENV, "   ")])).expect("parse");
        assert_eq!(blank, None);
    }

    #[test]
    fn environment_config_defaults_match_the_settings_form() {
        let parsed = from_lookup(lookup(&[(HOST_ENV, "core-mosquitto")]))
            .expect("parse")
            .expect("record");
        assert_eq!(parsed.port, DEFAULT_PORT);
        assert!(!parsed.tls);
        assert_eq!(parsed.username, None);
        assert_eq!(parsed.password, None);
        assert_eq!(parsed.base_topic, DEFAULT_BASE_TOPIC);
        assert_eq!(parsed.discovery_prefix, DEFAULT_DISCOVERY_PREFIX);
        assert_eq!(parsed.source, MqttConfigSource::Environment);
    }

    #[test]
    fn tls_without_an_explicit_port_uses_the_tls_default() {
        let parsed = from_lookup(lookup(&[(HOST_ENV, "broker"), (TLS_ENV, "true")]))
            .expect("parse")
            .expect("record");
        assert!(parsed.tls);
        assert_eq!(parsed.port, DEFAULT_TLS_PORT);
    }

    #[test]
    fn tls_accepts_the_spellings_a_shell_and_json_produce() {
        for value in ["1", "true", "TRUE", "yes", "on"] {
            let parsed = from_lookup(lookup(&[(HOST_ENV, "broker"), (TLS_ENV, value)]))
                .expect("parse")
                .expect("record");
            assert!(parsed.tls, "{value} should be true");
        }
        for value in ["0", "false", "no", "off", ""] {
            let parsed = from_lookup(lookup(&[(HOST_ENV, "broker"), (TLS_ENV, value)]))
                .expect("parse")
                .expect("record");
            assert!(!parsed.tls, "{value} should be false");
        }
    }

    #[test]
    fn unusable_values_are_reported_rather_than_ignored() {
        // Silently falling back would leave the add-on looking unconfigured
        // with nothing in the log to explain it - the exact failure #605 is
        // about.
        assert!(from_lookup(lookup(&[(HOST_ENV, "broker"), (PORT_ENV, "no")])).is_err());
        assert!(from_lookup(lookup(&[(HOST_ENV, "broker"), (TLS_ENV, "maybe")])).is_err());
    }

    #[test]
    fn a_password_keeps_its_surrounding_whitespace() {
        let parsed = from_lookup(lookup(&[(HOST_ENV, "broker"), (PASSWORD_ENV, " p a s s ")]))
            .expect("parse")
            .expect("record");
        assert_eq!(parsed.password.as_deref(), Some(" p a s s "));
    }

    #[test]
    fn environment_applies_when_nothing_is_stored() {
        assert_eq!(
            plan(None, Some(env_record())),
            MqttBootstrap::ApplyEnvironment(env_record())
        );
    }

    #[test]
    fn user_configuration_wins_over_the_environment() {
        // Even though the environment offers a perfectly good broker, the
        // user's own broker is what stays configured - and because the plan
        // is `UseStored`, nothing re-enables the publisher behind their back.
        assert_eq!(
            plan(Some(user_record()), Some(env_record())),
            MqttBootstrap::UseStored(user_record())
        );
    }

    #[test]
    fn a_user_record_wins_even_when_its_settings_match_the_environment() {
        // Same host/port/credentials, but the user saved them: provenance,
        // not content, decides. Otherwise a user who re-typed the add-on's
        // own broker would have their record treated as ours to overwrite.
        let mut stored = env_record();
        stored.source = MqttConfigSource::User;
        assert_eq!(
            plan(Some(stored.clone()), Some(env_record())),
            MqttBootstrap::UseStored(stored)
        );
    }

    #[test]
    fn re_applying_an_identical_environment_config_is_idempotent() {
        // No save, no re-enable: restarting must not churn the encrypted
        // credential file or resurrect a toggle the user switched off.
        assert_eq!(
            plan(Some(env_record()), Some(env_record())),
            MqttBootstrap::EnvironmentUnchanged(env_record())
        );
    }

    #[test]
    fn a_rotated_broker_password_is_re_applied() {
        let mut rotated = env_record();
        rotated.password = Some("rotated".to_string());
        assert_eq!(
            plan(Some(env_record()), Some(rotated.clone())),
            MqttBootstrap::ApplyEnvironment(rotated)
        );
    }

    #[test]
    fn a_previous_environment_record_survives_the_variables_going_away() {
        // Opting out (`publish_to_home_assistant: false`) stops the add-on
        // exporting the variables. Startup keeps the broker it already has
        // rather than deleting the user's encrypted record out from under
        // them; turning the publisher off is the Settings toggle's job.
        assert_eq!(
            plan(Some(env_record()), None),
            MqttBootstrap::UseStored(env_record())
        );
    }

    #[test]
    fn nothing_stored_and_nothing_in_the_environment_is_unconfigured() {
        assert_eq!(plan(None, None), MqttBootstrap::Unconfigured);
    }
}
