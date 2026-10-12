//! Build a user-reviewable device mismatch report (#774, alpha).
//!
//! Prior art this follows (see the fact-check in `docs/device-control-weave.md`):
//!
//! - **Home Assistant diagnostics**: redaction by structure — known sensitive keys replaced with
//!   the visible marker `**REDACTED**` — rather than by scanning text.
//! - **VS Code's issue reporter**: environment filled in automatically, a link to search existing
//!   issues, and GitHub's own new-issue page as the last screen before anything is sent.
//! - **Zigbee2MQTT device support**: captured device data becomes a contribution. Here the bundle
//!   has the shape of `tests/fixtures/devices/<family>/`, so a maintainer can drop it in as a
//!   fixture; it is labelled `user-captured`, never `hardware-verified`, until reviewed.
//!
//! Nothing here sends anything. The caller gets sections to preview, a prefilled issue URL the user
//! opens themselves, and a bundle the user may download and attach.

use std::collections::BTreeSet;

use serde::Serialize;
use serde_json::Value;

use super::anomalies::DeviceAnomaly;
use super::Device;

/// The redaction marker, identical to Home Assistant's diagnostics so it is recognisable.
pub const REDACTED: &str = "**REDACTED**";

/// Where reports are filed.
pub const ISSUE_REPO: &str = "open-horizon-labs/unified-hifi-control";

/// The issue form template (`.github/ISSUE_TEMPLATE/device-mismatch.yml`) and its report field.
/// Labels come from the template: the `labels` URL parameter needs triage permission and would
/// turn the link into a 404 for almost every user.
pub const ISSUE_TEMPLATE: &str = "device-mismatch.yml";
pub const ISSUE_REPORT_FIELD: &str = "report";

/// Budget for the prefilled URL. GitHub documents only that an oversized URL returns
/// `414 URI Too Long`, not the limit; this is a conservative assumption, not a GitHub number.
pub const URL_BUDGET_BYTES: usize = 7500;

/// JSON keys whose values are never shown (case-insensitive). Network identity, hardware
/// identity and user-chosen names.
const SENSITIVE_KEYS: &[&str] = &[
    "ssid",
    "essid",
    "bssid",
    "ap_mac",
    "apcli0",
    "eth0",
    "eth2",
    "ra0",
    "mac",
    "net_mac",
    "macaddress",
    "btmac",
    "eth_mac",
    "ip",
    "ipaddress",
    "ip_address",
    "host",
    "uuid",
    "upnp_uuid",
    "temp_uuid",
    "udn",
    "serial",
    "serialnumber",
    "serial_number",
    "device_id",
    "system_id",
    "unit_id",
    "name",
    "friendlyname",
    "friendly_name",
    "devicename",
    "groupname",
    "network_name",
];

/// One previewable part of a report.
#[derive(Debug, Clone, Serialize)]
pub struct ReportSection {
    pub id: &'static str,
    pub title: String,
    pub lines: Vec<String>,
    /// Whether the section is pre-selected in the preview. Raw protocol excerpts are opt-in.
    pub included_by_default: bool,
}

/// A complete, already-redacted report.
#[derive(Debug, Clone, Serialize)]
pub struct MismatchReport {
    pub title: String,
    /// `<family> <model>: <kinds>` — also what the duplicate search looks for.
    pub fingerprint: String,
    pub uhc_version: String,
    pub evidence: &'static str,
    pub sections: Vec<ReportSection>,
    /// What was redacted (key names and pattern kinds), so the preview can say so.
    pub redactions: Vec<String>,
    pub anomaly_count: usize,
}

/// The downloadable bundle: fixture-shaped and redacted.
#[derive(Debug, Clone, Serialize)]
pub struct MismatchBundle {
    pub provenance: Value,
    pub device: Value,
    pub anomalies: Vec<Value>,
}

/// Redact a JSON value in place by key, recursively. Returns the keys redacted.
fn redact_json(value: &mut Value, redacted: &mut BTreeSet<String>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                let sensitive = SENSITIVE_KEYS
                    .iter()
                    .any(|candidate| key.eq_ignore_ascii_case(candidate));
                let empty =
                    matches!(child, Value::Null) || child.as_str().is_some_and(str::is_empty);
                if sensitive && !empty {
                    *child = Value::String(REDACTED.to_string());
                    redacted.insert(format!("key:{key}"));
                } else {
                    redact_json(child, redacted);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                redact_json(item, redacted);
            }
        }
        Value::String(text) => {
            let (clean, kinds) = redact_text(text);
            *text = clean;
            redacted.extend(kinds);
        }
        _ => {}
    }
}

/// Redact free text: IPv4 addresses, MAC addresses (with or without separators) and UUIDs.
/// Used for line protocols (Telnet, eISCP), which have no keys to redact by.
pub fn redact_text(text: &str) -> (String, Vec<String>) {
    use std::sync::OnceLock;
    static PATTERNS: OnceLock<Vec<(regex::Regex, &'static str)>> = OnceLock::new();
    let patterns = PATTERNS.get_or_init(|| {
        [
            (
                r"\b(?:25[0-5]|2[0-4]\d|1?\d?\d)(?:\.(?:25[0-5]|2[0-4]\d|1?\d?\d)){3}\b",
                "pattern:ipv4",
            ),
            (r"(?i)\b[0-9a-f]{2}(?:[:-][0-9a-f]{2}){5}\b", "pattern:mac"),
            (
                r"(?i)\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b",
                "pattern:uuid",
            ),
            (r"(?i)\b[0-9a-f]{12}\b", "pattern:mac"),
            (r"(?i)\b[0-9a-f]{24}\b", "pattern:uuid"),
        ]
        .into_iter()
        .filter_map(|(pattern, kind)| regex::Regex::new(pattern).ok().map(|re| (re, kind)))
        .collect()
    });
    let mut out = text.to_string();
    let mut kinds = Vec::new();
    for (re, kind) in patterns {
        if re.is_match(&out) {
            out = re.replace_all(&out, REDACTED).into_owned();
            kinds.push((*kind).to_string());
        }
    }
    (out, kinds)
}

/// Redact an excerpt: structurally if it is JSON, by pattern otherwise.
pub fn redact_excerpt(excerpt: &str) -> (String, Vec<String>) {
    let mut redacted = BTreeSet::new();
    if let Ok(mut value) = serde_json::from_str::<Value>(excerpt) {
        redact_json(&mut value, &mut redacted);
        let text = serde_json::to_string(&value).unwrap_or_default();
        return (text, redacted.into_iter().collect());
    }
    // XML / line protocols: redact elements named like sensitive keys, then patterns.
    let mut text = excerpt.to_string();
    for key in SENSITIVE_KEYS {
        let re = regex::Regex::new(&format!(r"(?i)(<{key}>)[^<]+(</{key}>)"));
        if let Ok(re) = re {
            if re.is_match(&text) {
                text = re
                    .replace_all(&text, format!("${{1}}{REDACTED}${{2}}"))
                    .into_owned();
                redacted.insert(format!("key:{key}"));
            }
        }
    }
    let (text, kinds) = redact_text(&text);
    redacted.extend(kinds);
    (text, redacted.into_iter().collect())
}

/// The device facts a report carries: what a maintainer needs to reproduce, nothing that
/// identifies the user's network or names.
fn device_facts(device: &Device, redacted: &mut BTreeSet<String>) -> Value {
    let mut value = serde_json::json!({
        "family": device.device_id.kind(),
        "device_id": device.device_id.as_str(),
        "vendor": device.vendor,
        "model": device.model,
        "name": device.name,
        "host": device.host,
        "discovery": device.discovery,
        "maturity": device.maturity,
        "connection": device.connection,
        "power": device.power,
        "input": device.input,
        "inputs": device.inputs.iter().map(|input| serde_json::json!({
            "id": input.id, "name": input.name, "available": input.available,
        })).collect::<Vec<_>>(),
        "volume": device.volume,
        "capabilities": device.capabilities,
    });
    redact_json(&mut value, redacted);
    value
}

/// Build the report and bundle from one device's journal.
/// "Vendor model", without repeating a vendor the model name already carries ("WiiM WiiM Ultra").
fn vendor_model(vendor: &str, model: &str) -> String {
    if model.to_lowercase().starts_with(&vendor.to_lowercase()) {
        model.to_string()
    } else {
        format!("{vendor} {model}")
    }
}

pub fn build(
    device: &Device,
    anomalies: &[DeviceAnomaly],
    uhc_version: &str,
) -> (MismatchReport, MismatchBundle) {
    let mut redacted = BTreeSet::new();
    let facts = device_facts(device, &mut redacted);

    let kinds: BTreeSet<&str> = anomalies.iter().map(|a| a.kind.label()).collect();
    let fingerprint = format!(
        "{} {}: {}",
        device.device_id.kind(),
        device.model,
        if kinds.is_empty() {
            "no anomalies recorded".to_string()
        } else {
            kinds.iter().copied().collect::<Vec<_>>().join(", ")
        }
    );

    let mut anomaly_lines = Vec::new();
    let mut excerpt_lines = Vec::new();
    let mut bundle_anomalies = Vec::new();
    for anomaly in anomalies {
        let (detail, kinds) = redact_text(&anomaly.detail);
        redacted.extend(kinds);
        anomaly_lines.push(format!("- `{}` {}", anomaly.kind.label(), detail));
        let excerpt = anomaly.excerpt.as_deref().map(|raw| {
            let (clean, kinds) = redact_excerpt(raw);
            redacted.extend(kinds);
            clean
        });
        if let Some(excerpt) = &excerpt {
            excerpt_lines.push(format!("[{}] {}", anomaly.kind.label(), excerpt));
        }
        bundle_anomalies.push(serde_json::json!({
            "at_ms": anomaly.at_ms,
            "kind": anomaly.kind,
            "detail": detail,
            "excerpt": excerpt,
        }));
    }

    let environment = vec![
        format!("- UHC: {uhc_version}"),
        format!("- Family: `{}`", device.device_id.kind()),
        format!(
            "- Vendor / model: {}",
            vendor_model(&device.vendor, &device.model)
        ),
        format!("- Discovery: {}", device.discovery),
        format!("- Control channel: {:?}", device.connection.control_channel),
        format!(
            "- Volume: {:?} scale, min {}, hardware max {}, step {}",
            device.volume.scale,
            device.volume.min,
            device
                .volume
                .hardware_max
                .map_or_else(|| "unknown".to_string(), |max| max.to_string()),
            device.volume.step
        ),
    ];

    let sections = vec![
        ReportSection {
            id: "environment",
            title: "Device and UHC".to_string(),
            lines: environment,
            included_by_default: true,
        },
        ReportSection {
            id: "anomalies",
            title: format!("Mismatches ({})", anomalies.len()),
            lines: anomaly_lines,
            included_by_default: true,
        },
        ReportSection {
            id: "device_state",
            title: "Device state as UHC sees it (redacted)".to_string(),
            lines: serde_json::to_string_pretty(&facts)
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect(),
            included_by_default: true,
        },
        ReportSection {
            id: "excerpts",
            title: "Protocol excerpts (redacted)".to_string(),
            lines: excerpt_lines,
            // Raw exchanges are the most useful and the most sensitive: the user opts in.
            included_by_default: false,
        },
    ];

    let report = MismatchReport {
        title: format!("[device-mismatch] {fingerprint}"),
        fingerprint,
        uhc_version: uhc_version.to_string(),
        evidence: "user-captured",
        sections,
        redactions: redacted.into_iter().collect(),
        anomaly_count: anomalies.len(),
    };
    let bundle = MismatchBundle {
        provenance: serde_json::json!({
            "evidence": "user-captured",
            "uhc_version": uhc_version,
            "family": device.device_id.kind(),
            "model": device.model,
            "note": "Redacted device mismatch capture. A maintainer may promote it to \
                     tests/fixtures/devices/<family>/ after review; it is never hardware-verified \
                     on its own.",
        }),
        device: facts,
        anomalies: bundle_anomalies,
    };
    (report, bundle)
}

/// Render the selected sections as the issue body.
pub fn render_markdown(report: &MismatchReport, include: &[&str]) -> String {
    let mut out = format!(
        "_Generated by UHC {} from a user-reviewed, redacted capture. Evidence: {}._\n",
        report.uhc_version, report.evidence
    );
    for section in report
        .sections
        .iter()
        .filter(|section| include.contains(&section.id))
    {
        out.push_str(&format!("\n### {}\n\n", section.title));
        if section.id == "device_state" || section.id == "excerpts" {
            out.push_str("```\n");
            out.push_str(&section.lines.join("\n"));
            out.push_str("\n```\n");
        } else {
            out.push_str(&section.lines.join("\n"));
            out.push('\n');
        }
    }
    if !report.redactions.is_empty() {
        out.push_str(&format!("\n_Redacted: {}_\n", report.redactions.join(", ")));
    }
    out
}

fn encode(value: &str) -> String {
    urlencoding::encode(value).into_owned()
}

/// The prefilled issue URL. Sections are dropped whole (largest-optional first) until the URL
/// fits the budget; nothing is cut mid-line, and a dropped section is named in the body so the
/// maintainer knows to look in the attached bundle.
pub fn issue_url(report: &MismatchReport, include: &[&str]) -> (String, Vec<&'static str>) {
    let mut kept: Vec<&str> = include.to_vec();
    let mut dropped = Vec::new();
    let drop_order = ["excerpts", "device_state", "anomalies"];
    loop {
        let mut body = render_markdown(report, &kept);
        if !dropped.is_empty() {
            body.push_str(&format!(
                "\n_Too large for a link: {} omitted — see the attached bundle._\n",
                dropped.join(", ")
            ));
        }
        let url = format!(
            "https://github.com/{ISSUE_REPO}/issues/new?template={}&title={}&{}={}",
            encode(ISSUE_TEMPLATE),
            encode(&report.title),
            ISSUE_REPORT_FIELD,
            encode(&body)
        );
        if url.len() <= URL_BUDGET_BYTES {
            return (url, dropped);
        }
        let Some(next) = drop_order.iter().find(|id| kept.contains(id)).copied() else {
            return (url, dropped);
        };
        kept.retain(|id| *id != next);
        dropped.push(next);
    }
}

/// A GitHub search for existing reports with the same fingerprint.
pub fn search_url(report: &MismatchReport) -> String {
    format!(
        "https://github.com/{ISSUE_REPO}/issues?q={}",
        encode(&format!("is:issue \"{}\"", report.fingerprint))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::anomalies::AnomalyKind;
    use crate::devices::{
        CapabilityEvidence, ControlChannel, DeviceCapabilities, DeviceCapability, DeviceConnection,
        DeviceId, DeviceInput, DevicePower, DeviceVolume,
    };

    fn device() -> Device {
        let claim = DeviceCapability {
            supported: true,
            evidence: CapabilityEvidence::ImplementationDerived,
            detail: String::new(),
        };
        Device {
            device_id: DeviceId::parse("wiim:FF98F7F4BC71E2F81FF483E0").unwrap(),
            vendor: "WiiM".to_string(),
            model: "WiiM Ultra".to_string(),
            name: "Deck".to_string(),
            host: "192.168.1.201".to_string(),
            discovery: "mdns".to_string(),
            maturity: "alpha".to_string(),
            connection: DeviceConnection {
                control_channel: ControlChannel::Polling,
                detail: None,
            },
            power: DevicePower::Unknown,
            input: Some("wifi".to_string()),
            inputs: vec![DeviceInput {
                id: "line-in".to_string(),
                name: "Record Player".to_string(),
                available: true,
            }],
            volume: DeviceVolume {
                scale: crate::bus::VolumeScale::Percentage,
                value: Some(26.0),
                min: 0.0,
                hardware_max: Some(100.0),
                ceiling: None,
                step: 1.0,
                is_muted: Some(false),
                display: None,
            },
            capabilities: DeviceCapabilities {
                observe: claim.clone(),
                volume: claim.clone(),
                mute: claim.clone(),
                power: claim.clone(),
                input_select: claim,
            },
            revision: 1,
            last_updated: 0,
        }
    }

    /// The real WiiM `getStatusEx` capture, before its own redaction, carries the user's SSID,
    /// router BSSID/MAC and addresses. Reconstructed here with representative values.
    const RAW_STATUS_EX: &str = r#"{"ssid":"CastleHome","essid":"436173746C65486F6D65","BSSID":"a4:2b:b0:11:22:33","AP_MAC":"A4:2B:B0:11:22:33","eth2":"192.168.1.201","apcli0":"192.168.1.201","MAC":"54:78:C9:E4:69:28","uuid":"FF98F7F4BC71E2F81FF483E0","DeviceName":"Deck","project":"WiiM_Ultra","max_volume":"100","firmware":"Linkplay.5.2.829661"}"#;

    const SECRETS: &[&str] = &[
        "CastleHome",
        "436173746C65486F6D65",
        "a4:2b:b0:11:22:33",
        "A4:2B:B0:11:22:33",
        "192.168.1.201",
        "54:78:C9:E4:69:28",
        "FF98F7F4BC71E2F81FF483E0",
        "Deck",
        "Record Player",
    ];

    fn anomalies() -> Vec<DeviceAnomaly> {
        vec![
            DeviceAnomaly::new(
                DeviceId::parse("wiim:FF98F7F4BC71E2F81FF483E0").unwrap(),
                AnomalyKind::UnknownValue,
                "getPlayerStatus mode 47 maps to no input (unit 192.168.1.201)",
            )
            .with_excerpt(RAW_STATUS_EX),
            DeviceAnomaly::new(
                DeviceId::parse("marantz:0006781E0AB8").unwrap(),
                AnomalyKind::UnparsedLine,
                "MV line not understood",
            )
            .with_excerpt("MVX12\r@VOL:-395 from 192.168.1.229 mac 00:06:78:1E:0A:B8"),
        ]
    }

    /// CRITICAL: no network or personal identifier from a real capture survives into anything a
    /// user could send — preview sections, issue URL, or bundle. Fails a text-only scrub of the
    /// final body, which misses names and the hex-encoded SSID.
    #[test]
    fn critical_redaction_removes_every_identifier_everywhere() {
        let (report, bundle) = build(&device(), &anomalies(), "4.0.0-test");
        let everything = ["environment", "anomalies", "device_state", "excerpts"];
        let body = render_markdown(&report, &everything);
        let (url, _) = issue_url(&report, &everything);
        let decoded = urlencoding::decode(&url).unwrap().into_owned();
        let bundle_text = serde_json::to_string(&bundle).unwrap();
        let preview = serde_json::to_string(&report).unwrap();
        for secret in SECRETS {
            for (label, text) in [
                ("body", &body),
                ("url", &decoded),
                ("bundle", &bundle_text),
                ("preview", &preview),
            ] {
                assert!(!text.contains(secret), "{secret} leaked into the {label}");
            }
        }
        assert!(body.contains(REDACTED));
        assert!(report.redactions.iter().any(|r| r == "key:ssid"));
        // What a maintainer needs survives.
        assert!(bundle_text.contains("WiiM Ultra"));
        assert!(bundle_text.contains("Linkplay.5.2.829661"));
        assert!(body.contains("mode 47"));
    }

    /// The report is read by people: no Rust debug wrappers, no vendor printed twice.
    #[test]
    fn environment_reads_as_prose() {
        let (report, _) = build(&device(), &anomalies(), "4.0.0-test");
        let text = render_markdown(&report, &["environment"]);
        assert!(text.contains("Vendor / model: WiiM Ultra"), "{text}");
        assert!(text.contains("hardware max 100,"), "{text}");
        assert!(!text.contains("Some("), "{text}");
        let mut unknown = device();
        unknown.vendor = "Marantz".to_string();
        unknown.model = "SR6008".to_string();
        unknown.volume.hardware_max = None;
        let (report, _) = build(&unknown, &[], "4.0.0-test");
        let text = render_markdown(&report, &["environment"]);
        assert!(text.contains("Vendor / model: Marantz SR6008"), "{text}");
        assert!(text.contains("hardware max unknown"), "{text}");
    }

    #[test]
    fn raw_excerpts_are_opt_in() {
        let (report, _) = build(&device(), &anomalies(), "4.0.0-test");
        let excerpts = report.sections.iter().find(|s| s.id == "excerpts").unwrap();
        assert!(!excerpts.included_by_default);
        let defaults: Vec<&str> = report
            .sections
            .iter()
            .filter(|s| s.included_by_default)
            .map(|s| s.id)
            .collect();
        assert!(!render_markdown(&report, &defaults).contains("Protocol excerpts"));
    }

    /// The URL never exceeds the budget; sections are dropped whole and named, never truncated.
    #[test]
    fn oversized_reports_drop_whole_sections_and_say_so() {
        let many: Vec<DeviceAnomaly> = (0..500)
            .map(|n| {
                DeviceAnomaly::new(
                    DeviceId::parse("wiim:FF98F7F4BC71E2F81FF483E0").unwrap(),
                    AnomalyKind::UnparsedLine,
                    format!("unparsed payload number {n} with some descriptive detail"),
                )
                .with_excerpt("x".repeat(500))
            })
            .collect();
        let (report, _) = build(&device(), &many, "4.0.0-test");
        let (url, dropped) = issue_url(
            &report,
            &["environment", "anomalies", "device_state", "excerpts"],
        );
        assert!(url.len() <= URL_BUDGET_BYTES, "{}", url.len());
        assert!(dropped.contains(&"excerpts"));
        let decoded = urlencoding::decode(&url).unwrap().into_owned();
        assert!(decoded.contains("see the attached bundle"));
    }

    /// No `labels` parameter: it needs triage permission and 404s for ordinary users.
    #[test]
    fn the_issue_url_uses_the_template_not_labels() {
        let (report, _) = build(&device(), &anomalies(), "4.0.0-test");
        let (url, _) = issue_url(&report, &["environment"]);
        assert!(url.contains("template=device-mismatch.yml"));
        assert!(url.contains("&report="));
        assert!(!url.contains("labels="));
        assert!(search_url(&report).contains("issues?q="));
    }
}
