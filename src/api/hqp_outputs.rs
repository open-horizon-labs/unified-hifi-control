//! Shared HQPlayer output-routing command service.
//!
//! HTTP handlers, MCP tools and the browser all call these three functions with the same typed
//! request, so exact-instance targeting, expectation fences, correlation dedup and receipt shape
//! exist in exactly one place. Reads come from the aggregator's committed output projection and
//! writes go through the reliable command gateway to the exact-instance endpoint; nothing here
//! touches an adapter (`tests/adapter_boundary_lint.rs`).

use std::time::Duration;

use crate::adapters::hqplayer::outputs::{
    HqpOutputAvailability, HqpOutputCommandReceipt, HqpOutputCommandRequest, HqpOutputOperation,
    HqpOutputProjection, HqpOutputRefusal, HqpRelayConfigView, NaaRelaySettings, VIRTUAL_DEVICE_ID,
};
use crate::adapters::hqplayer::NAA_PROXY_COMPILED;
use crate::bus::runtime::{
    CommandDeadlines, CommandLane, CommandRequest, CommandStatus, CommandSubmissionError,
    HqpRuntimeCommand, RuntimeCommand,
};
use crate::bus::PrefixedZoneId;

use super::AppState;

/// Every way the service can decline, with the HTTP status and code each surface maps it to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HqpOutputServiceError {
    /// The zone id is not an exact `hqplayer:<instance>`.
    InvalidZone(String),
    /// No such HQPlayer instance is configured. Never falls back to another instance.
    UnknownInstance(String),
    /// A typed refusal from the fence, the gateway or the coordinator.
    Refused(HqpOutputRefusal),
    /// The reliable runtime is not composed (compatibility construction).
    RuntimeUnavailable,
    /// The command was accepted but no correlated projection committed in time.
    Indeterminate(String),
}

impl HqpOutputServiceError {
    pub fn status(&self) -> u16 {
        match self {
            Self::InvalidZone(_) => 400,
            Self::UnknownInstance(_) => 404,
            Self::Refused(refusal) => match refusal {
                HqpOutputRefusal::FeatureUnavailable
                | HqpOutputRefusal::NotYetImplemented { .. } => 501,
                HqpOutputRefusal::RelayDisabled
                | HqpOutputRefusal::StaleExpectation { .. }
                | HqpOutputRefusal::CorrelationConflict { .. } => 409,
                HqpOutputRefusal::RelayUnavailable { .. } => 503,
                HqpOutputRefusal::UnknownRoute { .. }
                | HqpOutputRefusal::UnknownOperation { .. } => 404,
                HqpOutputRefusal::InvalidCommand { .. } => 400,
                HqpOutputRefusal::Backend { .. } => 502,
            },
            Self::RuntimeUnavailable => 503,
            Self::Indeterminate(_) => 504,
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidZone(_) => "INVALID_ZONE_ID",
            Self::UnknownInstance(_) => "UNKNOWN_INSTANCE",
            Self::Refused(refusal) => refusal.code(),
            Self::RuntimeUnavailable => "RUNTIME_UNAVAILABLE",
            Self::Indeterminate(_) => "INDETERMINATE",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::InvalidZone(zone_id) => {
                format!("zone_id {zone_id:?} must be an exact hqplayer:<instance> zone id")
            }
            Self::UnknownInstance(instance) => {
                format!("HQPlayer instance '{instance}' is not configured")
            }
            Self::Refused(refusal) => refusal.message(),
            Self::RuntimeUnavailable => {
                "HQPlayer reliable command runtime is unavailable".to_string()
            }
            Self::Indeterminate(detail) => detail.clone(),
        }
    }
}

/// Resolve an exact `hqplayer:<instance>` zone id to its instance name.
pub fn parse_instance(zone_id: &str) -> Result<String, HqpOutputServiceError> {
    let Some(instance) = zone_id.strip_prefix("hqplayer:") else {
        return Err(HqpOutputServiceError::InvalidZone(zone_id.to_string()));
    };
    if instance.is_empty() || instance.contains(':') {
        return Err(HqpOutputServiceError::InvalidZone(zone_id.to_string()));
    }
    Ok(instance.to_string())
}

/// Whether the instance is known to this UHC: it has a published zone, a committed output
/// document, or a registered exact-instance endpoint. Nothing here consults an adapter.
async fn instance_known(state: &AppState, instance: &str) -> bool {
    let zone_id = format!("hqplayer:{instance}");
    if state.aggregator.get_zone(&zone_id).await.is_some()
        || state
            .aggregator
            .get_hqplayer_outputs(instance)
            .await
            .is_some()
    {
        return true;
    }
    state
        .reliable_commands
        .as_ref()
        .is_some_and(|gateway| gateway.has_endpoint(&PrefixedZoneId::hqplayer(instance)))
}

/// The document a build without the relay compiled in publishes for a known instance.
fn feature_unavailable_projection(instance: &str) -> HqpOutputProjection {
    let settings = NaaRelaySettings::default();
    HqpOutputProjection {
        zone_id: format!("hqplayer:{instance}"),
        instance: instance.to_string(),
        source_epoch: 0,
        aggregate_revision: 0,
        output_revision: 0,
        route_generation: 0,
        availability: HqpOutputAvailability::Unavailable {
            reason: HqpOutputRefusal::FeatureUnavailable.message(),
            since: 0,
        },
        relay: HqpRelayConfigView {
            enabled: false,
            adapter_name: settings.adapter_name,
            virtual_device_id: VIRTUAL_DEVICE_ID.to_string(),
            bind: None,
            hqp_allow: vec![],
            discovery_interface: None,
            discovery_port: settings.discovery_port,
            discovery_responder: None,
        },
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

/// `GET /hqplayer/outputs` / `hifi_hqplayer_outputs`: the committed document for an exact instance.
pub async fn read_hqp_outputs(
    state: &AppState,
    zone_id: &str,
) -> Result<HqpOutputProjection, HqpOutputServiceError> {
    let instance = parse_instance(zone_id)?;
    if let Some(projection) = state.aggregator.get_hqplayer_outputs(&instance).await {
        return Ok(projection);
    }
    if !instance_known(state, &instance).await {
        return Err(HqpOutputServiceError::UnknownInstance(instance));
    }
    if !NAA_PROXY_COMPILED {
        return Ok(feature_unavailable_projection(&instance));
    }
    // Known instance, relay compiled, but its worker has not published yet.
    let mut projection = feature_unavailable_projection(&instance);
    projection.availability = HqpOutputAvailability::Unavailable {
        reason:
            "the instance's output projection has not been committed yet (instance not started?)"
                .to_string(),
        since: 0,
    };
    Ok(projection)
}

/// `GET /hqplayer/outputs/operation` / `hifi_hqplayer_outputs{operation_id}`.
pub async fn read_hqp_output_operation(
    state: &AppState,
    zone_id: &str,
    operation_id: &str,
) -> Result<HqpOutputOperation, HqpOutputServiceError> {
    let projection = read_hqp_outputs(state, zone_id).await?;
    projection.operation(operation_id).cloned().ok_or_else(|| {
        HqpOutputServiceError::Refused(HqpOutputRefusal::UnknownOperation {
            operation_id: operation_id.to_string(),
        })
    })
}

/// Budget for a command's admission commit. Selection continues in the background after it.
const OUTPUT_CONFIRM_BUDGET: Duration = Duration::from_secs(15);

/// `POST /hqplayer/outputs/command` / `hifi_hqplayer_output_control`.
///
/// Order: exact instance → feature → expectation fence against the committed document →
/// correlation-scoped admission through the reliable gateway → wait for the correlated commit →
/// receipt built from the committed document.
pub async fn submit_hqp_output_command(
    state: &AppState,
    request: HqpOutputCommandRequest,
) -> Result<HqpOutputCommandReceipt, HqpOutputServiceError> {
    let instance = parse_instance(&request.zone_id)?;
    if !instance_known(state, &instance).await {
        return Err(HqpOutputServiceError::UnknownInstance(instance));
    }
    if !NAA_PROXY_COMPILED {
        return Err(HqpOutputServiceError::Refused(
            HqpOutputRefusal::FeatureUnavailable,
        ));
    }
    let Some(gateway) = state.reliable_commands.as_ref() else {
        return Err(HqpOutputServiceError::RuntimeUnavailable);
    };
    // Correlation is validated and resolved BEFORE any precondition: an exact retry of an already
    // executed command must return its original operation even though the mutation it performed
    // has since advanced the revision, and a different payload under the same id always conflicts.
    if let Some(correlation) = request.correlation_id.as_deref() {
        if correlation.trim().is_empty()
            || correlation.len() > 128
            || correlation.chars().any(char::is_control)
        {
            return Err(HqpOutputServiceError::Refused(
                HqpOutputRefusal::InvalidCommand {
                    message: "correlation_id must be 1–128 printable characters when supplied"
                        .to_string(),
                },
            ));
        }
    }
    let current = read_hqp_outputs(state, &request.zone_id).await?;
    if let Some(correlation) = request.correlation_id.as_deref() {
        let fingerprint = request.fingerprint();
        if let Some(existing) = current
            .operations
            .iter()
            .find(|o| o.correlation_id.as_deref() == Some(correlation))
        {
            if existing.request_fingerprint == fingerprint {
                return Ok(HqpOutputCommandReceipt {
                    accepted: true,
                    operation: existing.clone(),
                    projection: current,
                });
            }
            return Err(HqpOutputServiceError::Refused(
                HqpOutputRefusal::CorrelationConflict {
                    correlation_id: correlation.to_string(),
                },
            ));
        }
    }
    if request.action.requires_expectations()
        && (request.expected_source_epoch != Some(current.source_epoch)
            || request.expected_output_revision != Some(current.output_revision))
    {
        return Err(HqpOutputServiceError::Refused(
            HqpOutputRefusal::StaleExpectation {
                expected_source_epoch: request.expected_source_epoch,
                expected_output_revision: request.expected_output_revision,
                current_source_epoch: current.source_epoch,
                current_output_revision: current.output_revision,
            },
        ));
    }
    let correlation_id = request
        .correlation_id
        .as_deref()
        .map(|c| format!("hqp-output:{}:{c}", request.zone_id));
    let now = tokio::time::Instant::now();
    let mut ticket = gateway
        .submit(CommandRequest {
            target: PrefixedZoneId::hqplayer(&instance),
            command: RuntimeCommand::Hqplayer(HqpRuntimeCommand::Output(Box::new(request.clone()))),
            correlation_id: correlation_id.clone(),
            lane: CommandLane::Interactive,
            deadlines: CommandDeadlines {
                dispatch_by: now + Duration::from_secs(3),
                confirm_by: now + OUTPUT_CONFIRM_BUDGET,
            },
        })
        .await
        .map_err(|error| match error {
            CommandSubmissionError::CorrelationConflict(_) => {
                HqpOutputServiceError::Refused(HqpOutputRefusal::CorrelationConflict {
                    correlation_id: request.correlation_id.clone().unwrap_or_default(),
                })
            }
        })?;
    let operation_id = format!("op-{}", ticket.id().get());
    match ticket.wait_for_observable_result().await {
        CommandStatus::Confirmed { .. } => {}
        CommandStatus::Failed { detail } | CommandStatus::NotDispatched { detail } => {
            let refusal = HqpOutputRefusal::decode(&detail)
                .unwrap_or(HqpOutputRefusal::Backend { message: detail });
            return Err(HqpOutputServiceError::Refused(refusal));
        }
        CommandStatus::Indeterminate => {
            return Err(HqpOutputServiceError::Indeterminate(format!(
                "HQPlayer accepted output command {operation_id} but its projection did not commit in time; look it up with GET /hqplayer/outputs/operation"
            )));
        }
        CommandStatus::Queued | CommandStatus::Dispatched | CommandStatus::AwaitingProjection => {
            return Err(HqpOutputServiceError::Indeterminate(
                "HQPlayer output command stopped without a terminal result".to_string(),
            ));
        }
    }
    let projection = read_hqp_outputs(state, &request.zone_id).await?;
    let operation = projection.operation(&operation_id).cloned().ok_or_else(|| {
        HqpOutputServiceError::Indeterminate(format!(
            "output command {operation_id} confirmed but its record is not in the committed document"
        ))
    })?;
    Ok(HqpOutputCommandReceipt {
        accepted: true,
        operation,
        projection,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_exact_hqplayer_zone_ids_resolve() {
        assert_eq!(
            parse_instance("hqplayer:living").ok(),
            Some("living".to_string())
        );
        assert!(parse_instance("living").is_err());
        assert!(parse_instance("hqplayer:").is_err());
        assert!(parse_instance("roon:abc").is_err());
        assert!(parse_instance("hqplayer:a:b").is_err());
    }

    #[test]
    fn refusals_map_to_stable_status_codes() {
        assert_eq!(
            HqpOutputServiceError::Refused(HqpOutputRefusal::StaleExpectation {
                expected_source_epoch: None,
                expected_output_revision: None,
                current_source_epoch: 0,
                current_output_revision: 0,
            })
            .status(),
            409
        );
        assert_eq!(
            HqpOutputServiceError::UnknownInstance("x".into()).status(),
            404
        );
        assert_eq!(
            HqpOutputServiceError::Refused(HqpOutputRefusal::FeatureUnavailable).status(),
            501
        );
        assert_eq!(
            HqpOutputServiceError::Refused(HqpOutputRefusal::RelayUnavailable {
                reason: "x".into()
            })
            .status(),
            503
        );
    }
}
