//! Provider command primitives shared by client surfaces and zone control.
//! Canonical state comes from the aggregator; native operations use the reliable runtime.
use crate::aggregator::ZoneAggregator;
use crate::bus::runtime::{
    CommandDeadlines, CommandGateway, CommandLane, CommandRequest, CommandStatus, RuntimeCommand,
};
use crate::bus::{Command, PrefixedZoneId};
use std::time::Duration;

/// Same routing and readback wait as [`crate::api::dispatch_lms_runtime_command`], parameterized directly over
/// the reliable command gateway so callers without a full `AppState` - MQTT's inbound command
/// router (#529) - reuse the identical dispatch path HTTP/knob/MCP use.
pub(crate) async fn dispatch_lms_runtime_command_via(
    gateway: Option<&CommandGateway>,
    zone_id: &str,
    command: Command,
) -> anyhow::Result<()> {
    let target = PrefixedZoneId::lms(zone_id.strip_prefix("lms:").unwrap_or(zone_id));
    let Some(gateway) = gateway else {
        // Compatibility-only construction used by older embedders and contract fixtures. There is
        // no direct adapter fallback: preserve the established actionable error while refusing
        // native I/O outside the reliable runtime.
        return Err(anyhow::anyhow!("LMS host not configured"));
    };
    if !gateway.has_endpoint(&target) {
        return Err(anyhow::anyhow!("LMS command endpoint is not available"));
    }
    let now = tokio::time::Instant::now();
    let dispatch_by = crate::command_dispatch::deadline();
    let mut ticket = gateway
        .submit(CommandRequest {
            target,
            command: RuntimeCommand::Control(command),
            correlation_id: None,
            lane: CommandLane::Interactive,
            deadlines: CommandDeadlines {
                dispatch_by,
                confirm_by: now + Duration::from_secs(15),
            },
        })
        .await
        .map_err(|error| anyhow::anyhow!("LMS command admission failed: {error:?}"))?;
    match ticket.wait_for_observable_result().await {
        CommandStatus::Confirmed { .. } => Ok(()),
        CommandStatus::Failed { detail } | CommandStatus::ItemUnavailable { detail } => {
            Err(anyhow::anyhow!(detail))
        }
        CommandStatus::NotDispatched { detail } => {
            if tokio::time::Instant::now() >= dispatch_by {
                Err(crate::command_dispatch::DispatchExpired.into())
            } else {
                Err(anyhow::anyhow!(detail))
            }
        }
        CommandStatus::Indeterminate => Err(anyhow::anyhow!(
            "LMS accepted the command but did not publish a verified readback in time"
        )),
        CommandStatus::Queued | CommandStatus::Dispatched | CommandStatus::AwaitingProjection => {
            Err(anyhow::anyhow!(
                "LMS command stopped without a terminal result"
            ))
        }
    }
}

/// Same routing as [`crate::api::dispatch_openhome_runtime_command`], parameterized directly over the reliable
/// command gateway so callers without a full `AppState` - MQTT's inbound command router (#529) -
/// reuse the identical dispatch path HTTP/knob/MCP use.
pub(crate) async fn dispatch_openhome_runtime_command_via(
    gateway: Option<&CommandGateway>,
    zone_id: &str,
    command: Command,
) -> anyhow::Result<()> {
    if gateway.is_none() {
        let raw_id = zone_id.strip_prefix("openhome:").unwrap_or(zone_id);
        return Err(anyhow::anyhow!("Device not found: {raw_id}"));
    }
    dispatch_provider_runtime_command(
        gateway,
        PrefixedZoneId::openhome(zone_id.strip_prefix("openhome:").unwrap_or(zone_id)),
        command,
        "OpenHome",
    )
    .await
}

/// Same routing as [`crate::api::dispatch_upnp_runtime_command`], parameterized directly over the reliable
/// command gateway so callers without a full `AppState` - MQTT's inbound command router (#529) -
/// reuse the identical dispatch path HTTP/knob/MCP use.
pub(crate) async fn dispatch_upnp_runtime_command_via(
    gateway: Option<&CommandGateway>,
    zone_id: &str,
    command: Command,
) -> anyhow::Result<()> {
    // Standalone API/MCP fixtures intentionally compose no runtime. Preserve
    // the frozen adapter-shaped refusal without doing native I/O outside the
    // composed server's reliable endpoint.
    if gateway.is_none() {
        let raw_id = zone_id.strip_prefix("upnp:").unwrap_or(zone_id);
        return Err(anyhow::anyhow!("Renderer not found: {raw_id}"));
    }
    dispatch_provider_runtime_command(
        gateway,
        PrefixedZoneId::upnp(zone_id.strip_prefix("upnp:").unwrap_or(zone_id)),
        command,
        "UPnP",
    )
    .await
}

/// Same routing as [`crate::api::dispatch_roon_runtime_command`], parameterized directly over the reliable
/// command gateway so callers without a full `AppState` - MQTT's inbound command router (#529) -
/// reuse the identical dispatch path HTTP/knob/MCP use.
pub(crate) async fn dispatch_roon_runtime_command_via(
    gateway: Option<&CommandGateway>,
    zone_id: &str,
    command: Command,
) -> anyhow::Result<()> {
    if gateway.is_none() {
        return Err(anyhow::anyhow!("Not connected to Roon"));
    }
    dispatch_provider_runtime_command(
        gateway,
        PrefixedZoneId::roon(zone_id.strip_prefix("roon:").unwrap_or(zone_id)),
        command,
        "Roon",
    )
    .await
}

async fn dispatch_provider_runtime_command(
    gateway: Option<&CommandGateway>,
    target: PrefixedZoneId,
    command: Command,
    provider: &str,
) -> anyhow::Result<()> {
    let Some(gateway) = gateway else {
        return Err(anyhow::anyhow!(
            "{provider} reliable command runtime is unavailable"
        ));
    };
    if !gateway.has_endpoint(&target) {
        return Err(anyhow::anyhow!(
            "{provider} command endpoint is not available"
        ));
    }
    let now = tokio::time::Instant::now();
    let dispatch_by = crate::command_dispatch::deadline();
    let mut ticket = gateway
        .submit(CommandRequest {
            target,
            command: RuntimeCommand::Control(command),
            correlation_id: None,
            lane: CommandLane::Interactive,
            deadlines: CommandDeadlines {
                dispatch_by,
                confirm_by: now + Duration::from_secs(15),
            },
        })
        .await
        .map_err(|error| anyhow::anyhow!("{provider} command admission failed: {error:?}"))?;
    match ticket.wait_for_observable_result().await {
        CommandStatus::Confirmed { .. } => Ok(()),
        CommandStatus::Failed { detail } | CommandStatus::ItemUnavailable { detail } => {
            Err(anyhow::anyhow!(detail))
        }
        CommandStatus::NotDispatched { detail } => {
            if tokio::time::Instant::now() >= dispatch_by {
                Err(crate::command_dispatch::DispatchExpired.into())
            } else {
                Err(anyhow::anyhow!(detail))
            }
        }
        CommandStatus::Indeterminate => Err(anyhow::anyhow!(
            "{provider} accepted the command but did not publish a verified readback in time"
        )),
        CommandStatus::Queued | CommandStatus::Dispatched | CommandStatus::AwaitingProjection => {
            Err(anyhow::anyhow!(
                "{provider} command stopped without a terminal result"
            ))
        }
    }
}

/// Control a direct HQPlayer instance (#328).
///
/// **Every refusal below is checked against the zone the aggregator published**, not against a fresh
/// adapter read. That is deliberate and it is the invariant that keeps this function honest: the
/// capability flag a client was handed in `GET /knob/now_playing` is literally the flag its command
/// is judged by, so "advertised" and "permitted" cannot drift apart. It is also required — a surface
/// may not query an adapter for state (`docs/ARCHITECTURE.md`, `tests/architecture_lint.rs`).
///
/// Capability decisions use the last published snapshot. After a successful write, the managed
/// adapter performs a coherent readback and publishes it through the aggregator before this
/// function reports success, so every surface converges on the same post-command state.
/// Transport-neutral outcome of [`dispatch_hqplayer_action`].
///
/// One dispatch function is shared by every surface that lets a caller name an arbitrary zone id
/// and an action string — today this HTTP handler and MCP's `hifi_control` tool
/// (`src/mcp/mod.rs`) — so the capability checks, clamps and command core exist in exactly one
/// place. Each surface converts this into its own wire shape.
pub(crate) enum HqpDispatchError {
    /// The named instance, or the zone the aggregator currently publishes for it, does not exist.
    NotFound(String),
    /// The action, value, or current zone state make this specific request invalid.
    BadRequest { message: String, code: &'static str },
    /// The adapter accepted the request but the native command itself failed.
    Backend(String),
}

impl HqpDispatchError {
    pub(crate) fn message(&self) -> &str {
        match self {
            Self::NotFound(message) | Self::Backend(message) => message,
            Self::BadRequest { message, .. } => message,
        }
    }
}

/// Same aggregator-gated routing as [`dispatch_hqplayer_action`], parameterized directly over the
/// aggregator and reliable command gateway so callers without a full `AppState` - MQTT's inbound
/// command router (#529) - reuse the identical dispatch path HTTP/knob/MCP use.
pub(crate) async fn dispatch_hqplayer_action_via(
    aggregator: &ZoneAggregator,
    gateway: Option<&CommandGateway>,
    zone_id: &str,
    instance: &str,
    action: &str,
    value: Option<f64>,
) -> Result<(), HqpDispatchError> {
    // The aggregator is the only state source consulted here.
    //
    // Its absence is decisive rather than merely inconvenient: the aggregator withdraws the zone when
    // the producer stops, so a zone it does not hold is one no surface will show and none of this
    // function's capability checks can be evaluated against. The transport arms below consulted the
    // zone only for `next`/`previous`, so `play`, `pause` and `stop` were accepted — and answered
    // `{"ok":true}` — for a withdrawn zone whose instance still happened to exist in the manager's
    // map. Found by the review pass, pinned by
    // `transport_is_refused_for_a_zone_the_aggregator_has_withdrawn`.
    let Some(zone) = aggregator.get_zone(zone_id).await else {
        let target = PrefixedZoneId::hqplayer(instance);
        let message = match gateway {
            Some(gateway) if gateway.has_endpoint(&target) => {
                format!("zone {zone_id} is not currently published")
            }
            _ => format!("HQPlayer instance '{instance}' is not configured"),
        };
        return Err(HqpDispatchError::NotFound(message));
    };

    let command = hqp_command_from_published_zone(&zone, action, value)?;
    dispatch_hqplayer_runtime_command_via(
        aggregator,
        gateway,
        instance,
        RuntimeCommand::Control(command),
        CommandLane::Interactive,
        std::time::Duration::from_secs(10),
    )
    .await
}

/// Same aggregator-gated routing as [`crate::knobs::routes::dispatch_hqplayer_runtime_command`], parameterized directly
/// over the aggregator and reliable command gateway so callers without a full `AppState` - MQTT's
/// inbound command router (#529) - reuse the identical dispatch path HTTP/knob/MCP use.
pub(crate) async fn dispatch_hqplayer_runtime_command_via(
    aggregator: &ZoneAggregator,
    gateway: Option<&CommandGateway>,
    instance: &str,
    command: RuntimeCommand,
    lane: CommandLane,
    confirmation_budget: std::time::Duration,
) -> Result<(), HqpDispatchError> {
    let target = PrefixedZoneId::hqplayer(instance);
    let zone_id = target.to_string();
    if aggregator.get_zone(&zone_id).await.is_none() {
        let message = match gateway {
            Some(gateway) if gateway.has_endpoint(&target) => {
                format!("zone {zone_id} is not currently published")
            }
            _ => format!("HQPlayer instance '{instance}' is not configured"),
        };
        return Err(HqpDispatchError::NotFound(message));
    }
    let Some(gateway) = gateway else {
        return Err(HqpDispatchError::Backend(
            "HQPlayer reliable command runtime is unavailable".to_string(),
        ));
    };
    let now = tokio::time::Instant::now();
    let dispatch_by = crate::command_dispatch::deadline();
    let mut ticket = gateway
        .submit(CommandRequest {
            target,
            command,
            correlation_id: None,
            lane,
            deadlines: CommandDeadlines {
                dispatch_by,
                confirm_by: now + confirmation_budget,
            },
        })
        .await
        .map_err(|error| {
            HqpDispatchError::Backend(format!("HQPlayer command admission failed: {error:?}"))
        })?;
    match ticket.wait_for_observable_result().await {
        CommandStatus::Confirmed { .. } => Ok(()),
        CommandStatus::Failed { detail } | CommandStatus::ItemUnavailable { detail } => {
            Err(HqpDispatchError::Backend(detail))
        }
        CommandStatus::NotDispatched { detail } => Err(HqpDispatchError::Backend(
            if tokio::time::Instant::now() >= dispatch_by {
                crate::command_dispatch::EXPIRED_MESSAGE.into()
            } else {
                detail
            },
        )),
        CommandStatus::Indeterminate => Err(HqpDispatchError::Backend(
            "HQPlayer accepted the command but did not publish a verified readback in time"
                .to_string(),
        )),
        CommandStatus::Queued | CommandStatus::Dispatched | CommandStatus::AwaitingProjection => {
            Err(HqpDispatchError::Backend(
                "HQPlayer command stopped without a terminal result".to_string(),
            ))
        }
    }
}

/// Translate only a request that the aggregator's published zone says is currently valid.  This
/// mirrors the legacy direct path below, but returns a semantic command for the endpoint worker so
/// no client surface gets its own HQPlayer transport implementation.
fn hqp_command_from_published_zone(
    zone: &crate::bus::Zone,
    action: &str,
    value: Option<f64>,
) -> Result<Command, HqpDispatchError> {
    match action {
        "play" => Ok(Command::Play),
        "pause" => Ok(Command::Pause),
        "stop" => Ok(Command::Stop),
        "next" if zone.is_next_allowed => Ok(Command::Next),
        "previous" | "prev" if zone.is_previous_allowed => Ok(Command::Previous),
        "next" | "previous" | "prev" => Err(HqpDispatchError::BadRequest {
            message: format!("{action} is not available in the zone's current state"),
            code: "ACTION_NOT_ALLOWED",
        }),
        // Resolve at the serialized native endpoint. Concurrent requests can otherwise all observe
        // one published state and collapse several toggles into the same Play or Pause command.
        "play_pause" | "playpause" => Ok(Command::PlayPause),
        "seek" => {
            if !zone.is_seekable {
                return Err(HqpDispatchError::BadRequest {
                    message: "this zone is not seekable in its current state".to_string(),
                    code: "ACTION_NOT_ALLOWED",
                });
            }
            let Some(position) = value else {
                return Err(HqpDispatchError::BadRequest {
                    message: "seek requires a numeric position in seconds".to_string(),
                    code: "INVALID_VALUE",
                });
            };
            if !position.is_finite() || position < 0.0 {
                return Err(HqpDispatchError::BadRequest {
                    message: "seek position must be a non-negative number of seconds".to_string(),
                    code: "INVALID_VALUE",
                });
            }
            let ceiling = zone
                .now_playing
                .as_ref()
                .and_then(|np| np.duration)
                .filter(|duration| *duration > 0.0);
            Ok(Command::Seek {
                position: ceiling.map_or(position, |duration| position.min(duration)),
            })
        }
        "vol_up" | "volume_up" | "vol_down" | "volume_down" => {
            let vc = require_volume_control(Some(zone))?;
            let step = value
                .map(f64::abs)
                .filter(|step| step.is_finite() && *step > 0.0)
                .unwrap_or(f64::from(vc.step));
            let delta = if action.contains("down") { -step } else { step };
            Ok(Command::VolumeRelative {
                // The endpoint resolves this delta against native State after it reaches the head
                // of the serialized queue. Resolving it here against `vc.value` collapses concurrent
                // knob turns that all observed the same pre-command projection into one step.
                delta: quantise_db(delta) as f32,
                output_id: None,
            })
        }
        "vol_abs" | "volume" => {
            let vc = require_volume_control(Some(zone))?;
            let Some(value) = value.filter(|value| value.is_finite()) else {
                return Err(HqpDispatchError::BadRequest {
                    message: "an absolute volume requires a finite numeric level in dB".to_string(),
                    code: "INVALID_VALUE",
                });
            };
            Ok(Command::VolumeAbsolute {
                value: quantise_db(value.clamp(f64::from(vc.min), f64::from(vc.max))) as f32,
                output_id: None,
            })
        }
        "mute" => {
            require_volume_control(Some(zone))?;
            Ok(Command::Mute {
                muted: true,
                output_id: None,
            })
        }
        _ => Err(HqpDispatchError::BadRequest {
            message: format!("Unknown action: {action}"),
            code: "UNKNOWN_ACTION",
        }),
    }
}

/// Round a computed dB level to a hundredth of a decibel before it reaches the wire.
///
/// The published zone carries `value`, `min`, `max` and `step` as `f32` (`crate::bus::VolumeControl`),
/// so widening them back to `f64` to do arithmetic reintroduces the representation error as trailing
/// digits. A daemon reporting a 0.1 dB step turned `-23.5 + 0.1` into `-23.399999998509884`, and
/// `set_volume_db` formats with `{}` — so that is what would have gone out on the wire, where the
/// reference client sends `-23.4`.
///
/// A hundredth of a dB is far below audibility and below the finest step any observed daemon reports,
/// so this cannot quantise away a real distinction; it only removes float noise. It is deliberately
/// **not** a clamp and does not decide any bound — the clamp against the zone's observed range has
/// already happened by the time this is called.
fn quantise_db(db: f64) -> f64 {
    (db * 100.0).round() / 100.0
}

/// The zone's published volume control, or a refusal.
///
/// A zone with no volume control is a zone whose daemon answers `Volume` with a bare
/// `result="Error"`. Sending one anyway is a request with no safe interpretation on a
/// hardware-attenuated setup, so it is refused before it reaches the wire.
///
/// The bounds are re-checked here even though the projection already refuses an unorderable range
/// (`HqpAdapter::volume_range_is_usable`). Both `.clamp` calls below panic on `min > max` or a NaN
/// bound, and a panic in an HTTP or MCP handler is a worse failure than a 400: this function is the
/// only gate in front of them, so it owns the precondition rather than trusting its caller's caller.
fn require_volume_control(
    zone: Option<&crate::bus::Zone>,
) -> Result<crate::bus::VolumeControl, HqpDispatchError> {
    let refuse = |message: &str| HqpDispatchError::BadRequest {
        message: message.to_string(),
        code: "VOLUME_NOT_AVAILABLE",
    };
    let control = zone
        .and_then(|z| z.volume_control.clone())
        .ok_or_else(|| refuse("this zone has no volume control"))?;
    if !(control.min.is_finite() && control.max.is_finite() && control.min < control.max) {
        return Err(refuse(
            "this zone's published volume range cannot bound a level",
        ));
    }
    Ok(control)
}
