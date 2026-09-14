//! One-time HQPlayer-side setup: point Embedded's **running** output at the managed relay.
//!
//! Evidenced flow (private Embedded 6.0.4 record, 2026-09-14; nothing from it is copied here):
//! the persistent web lane's `/config` page is one HTML form. Its *successful controls* are read,
//! `backend` becomes `network` and `net_device` becomes the exact option HQPlayer itself offers
//! for the relay (`"<adapter name>/<virtual device id>"`, discovered by HQPlayer's own NAA scan),
//! and the whole form is posted back. `POST /restore` only rewrites the on-disk XML and does not
//! change the running selection, so rollback re-posts the previously read form first and restores
//! the raw persistent bytes second. Nobody supplies XML or attribute maps; DSP controls travel
//! back exactly as read.

use sha2::{Digest, Sha256};

use super::outputs::{HqpSetupAttribute, HqpSetupChange, HqpSetupPreview};

/// `backend` value HQPlayer's form uses for network audio output (observed live).
pub const BACKEND_NETWORK: &str = "network";
/// Network output bit depth applied when the form still carries the unset value.
pub const DEFAULT_NET_BITS: &str = "32";
/// Network output period (ms) applied when the form still carries the unset value.
pub const DEFAULT_NET_PERIOD: &str = "250";
/// Controls shown in the preview's `current` list.
const OUTPUT_CONTROLS: [&str; 6] = [
    "backend",
    "net_device",
    "alsa_device",
    "net_bits",
    "net_period",
    "mode",
];

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// One `<select>` in the form: its name and every option value HQPlayer offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectInventory {
    pub name: String,
    pub options: Vec<String>,
}

/// The first form's successful controls in document order, plus select inventories.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConfigForm {
    pub fields: Vec<(String, String)>,
    pub selects: Vec<SelectInventory>,
}

impl ConfigForm {
    pub fn value(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn options(&self, name: &str) -> Option<&[String]> {
        self.selects
            .iter()
            .find(|s| s.name == name)
            .map(|s| s.options.as_slice())
    }

    /// Identity of the successful controls, for the "unchanged since preview" fence.
    pub fn fingerprint(&self) -> String {
        sha256_hex(encode(&self.fields).as_bytes())
    }
}

fn unescape(text: &str) -> String {
    text.replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut search = 0;
    while let Some(pos) = lower[search..].find(name) {
        let at = search + pos;
        let before_ok = at == 0
            || lower.as_bytes()[at - 1].is_ascii_whitespace()
            || lower.as_bytes()[at - 1] == b'<';
        let rest = &tag[at + name.len()..];
        if before_ok {
            let trimmed = rest.trim_start();
            if let Some(after_eq) = trimmed.strip_prefix('=') {
                let after_eq = after_eq.trim_start();
                let quote = after_eq.chars().next()?;
                if quote == '"' || quote == '\'' {
                    let end = after_eq[1..].find(quote)?;
                    return Some(unescape(&after_eq[1..1 + end]));
                }
                let end = after_eq
                    .find(|c: char| c.is_whitespace() || c == '>' || c == '/')
                    .unwrap_or(after_eq.len());
                return Some(unescape(&after_eq[..end]));
            }
            if rest.is_empty()
                || rest.starts_with(|c: char| c.is_whitespace() || c == '>' || c == '/')
            {
                return Some(String::new());
            }
        }
        search = at + name.len();
    }
    None
}

fn has_flag(tag: &str, name: &str) -> bool {
    attr(tag, name).is_some()
}

/// Parse the first `<form>` of a `/config` page into its successful controls, following the
/// HTML form submission algorithm: named, enabled inputs except buttons/files; checked
/// checkboxes/radios only; a select contributes its selected option (or its first option when
/// none is marked and it is not `multiple`); textareas contribute their text.
pub fn parse_form(html: &str) -> Result<ConfigForm, String> {
    let lower = html.to_ascii_lowercase();
    let start = lower
        .find("<form")
        .ok_or("the configuration page carries no form")?;
    let end = lower[start..]
        .find("</form>")
        .map(|e| start + e)
        .unwrap_or(html.len());
    let body = &html[start..end];
    let mut form = ConfigForm::default();
    let mut i = 0;
    let bytes = body.as_bytes();
    while let Some(open) = body[i..].find('<') {
        let tag_start = i + open;
        let Some(close) = body[tag_start..].find('>') else {
            break;
        };
        let tag_end = tag_start + close + 1;
        let tag = &body[tag_start..tag_end];
        let tag_lower = tag.to_ascii_lowercase();
        if tag_lower.starts_with("<input") {
            if let Some(name) = attr(tag, "name").filter(|n| !n.is_empty()) {
                let kind = attr(tag, "type")
                    .unwrap_or_else(|| "text".into())
                    .to_ascii_lowercase();
                let disabled = has_flag(tag, "disabled");
                if !disabled
                    && !matches!(
                        kind.as_str(),
                        "submit" | "button" | "reset" | "file" | "image"
                    )
                {
                    if matches!(kind.as_str(), "checkbox" | "radio") {
                        if has_flag(tag, "checked") {
                            form.fields
                                .push((name, attr(tag, "value").unwrap_or_else(|| "on".into())));
                        }
                    } else {
                        form.fields
                            .push((name, attr(tag, "value").unwrap_or_default()));
                    }
                }
            }
            i = tag_end;
        } else if tag_lower.starts_with("<select") {
            let Some(sel_close) = body[tag_end..].to_ascii_lowercase().find("</select>") else {
                break;
            };
            let inner = &body[tag_end..tag_end + sel_close];
            let name = attr(tag, "name").unwrap_or_default();
            let disabled = has_flag(tag, "disabled");
            let multiple = has_flag(tag, "multiple");
            let mut options: Vec<(String, bool)> = Vec::new();
            let mut j = 0;
            let inner_lower = inner.to_ascii_lowercase();
            while let Some(o) = inner_lower[j..].find("<option") {
                let o_start = j + o;
                let Some(o_close) = inner[o_start..].find('>') else {
                    break;
                };
                let o_tag = &inner[o_start..o_start + o_close + 1];
                let text_start = o_start + o_close + 1;
                let text_end = inner_lower[text_start..]
                    .find("</option>")
                    .map(|e| text_start + e)
                    .or_else(|| {
                        inner_lower[text_start..]
                            .find("<option")
                            .map(|e| text_start + e)
                    })
                    .unwrap_or(inner.len());
                let text = unescape(inner[text_start..text_end].trim());
                let value = attr(o_tag, "value").unwrap_or(text);
                options.push((value, has_flag(o_tag, "selected")));
                j = text_end;
            }
            if !name.is_empty() {
                let mut selected: Vec<String> = options
                    .iter()
                    .filter(|(_, sel)| *sel)
                    .map(|(v, _)| v.clone())
                    .collect();
                if selected.is_empty() && !multiple {
                    if let Some((first, _)) = options.first() {
                        selected.push(first.clone());
                    }
                }
                if !disabled {
                    for value in selected {
                        form.fields.push((name.clone(), value));
                    }
                }
                form.selects.push(SelectInventory {
                    name,
                    options: options.into_iter().map(|(v, _)| v).collect(),
                });
            }
            i = tag_end + sel_close + "</select>".len();
        } else if tag_lower.starts_with("<textarea") {
            let Some(ta_close) = body[tag_end..].to_ascii_lowercase().find("</textarea>") else {
                break;
            };
            if let Some(name) = attr(tag, "name").filter(|n| !n.is_empty()) {
                if !has_flag(tag, "disabled") {
                    form.fields
                        .push((name, unescape(&body[tag_end..tag_end + ta_close])));
                }
            }
            i = tag_end + ta_close + "</textarea>".len();
        } else {
            i = tag_end;
        }
        if i >= bytes.len() {
            break;
        }
    }
    if form.fields.is_empty() {
        return Err("the configuration form carries no successful controls".into());
    }
    Ok(form)
}

/// `application/x-www-form-urlencoded` body in control order.
pub fn encode(fields: &[(String, String)]) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (name, value) in fields {
        serializer.append_pair(name, value);
    }
    serializer.finish()
}

/// The exact `net_device` option value HQPlayer offers for a relay advertising `adapter_name`
/// with `virtual_device_id`.
pub fn relay_option(adapter_name: &str, virtual_device_id: &str) -> String {
    format!("{adapter_name}/{virtual_device_id}")
}

/// Derived proposal for `form`: the current output controls, the changes that select the relay,
/// and the complete field list apply would post (every other control preserved verbatim).
pub fn derive(
    form: &ConfigForm,
    backup: &[u8],
    adapter_name: &str,
    virtual_device_id: &str,
) -> (HqpSetupPreview, Vec<(String, String)>) {
    let backup_sha256 = sha256_hex(backup);
    let current: Vec<HqpSetupAttribute> = OUTPUT_CONTROLS
        .iter()
        .filter_map(|name| {
            form.value(name).map(|value| HqpSetupAttribute {
                name: name.to_string(),
                value: value.to_string(),
            })
        })
        .collect();
    let wanted = relay_option(adapter_name, virtual_device_id);
    let mut blockers = Vec::new();
    let backend_options = form.options("backend").unwrap_or(&[]);
    if !backend_options.iter().any(|o| o == BACKEND_NETWORK) {
        blockers.push(format!(
            "the form offers no backend={BACKEND_NETWORK:?} option ({} options seen)",
            backend_options.len()
        ));
    }
    let device_options = form.options("net_device").unwrap_or(&[]);
    let relay_found = device_options.iter().any(|o| *o == wanted);
    if !relay_found {
        blockers.push(format!(
            "HQPlayer has not discovered the relay as net_device {wanted:?}; enable discovery on the relay (same port as HQPlayer's scan) or refresh devices in HQPlayer ({} network devices offered)",
            device_options.len()
        ));
    }
    let mut proposed: Vec<(String, String)> = form.fields.clone();
    let mut changes = Vec::new();
    let mut set = |name: &str, to: &str, changes: &mut Vec<HqpSetupChange>| {
        let from = proposed
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.clone());
        if from.as_deref() == Some(to) {
            return;
        }
        match proposed.iter_mut().find(|(n, _)| n == name) {
            Some(slot) => slot.1 = to.to_string(),
            None => proposed.push((name.to_string(), to.to_string())),
        }
        changes.push(HqpSetupChange {
            attribute: name.to_string(),
            from,
            to: to.to_string(),
        });
    };
    set("backend", BACKEND_NETWORK, &mut changes);
    set("net_device", &wanted, &mut changes);
    // Network output framing defaults only where the form still carries the unset value; a
    // deliberately configured value is preserved.
    for (name, default) in [
        ("net_bits", DEFAULT_NET_BITS),
        ("net_period", DEFAULT_NET_PERIOD),
    ] {
        if matches!(form.value(name), Some("0") | Some("")) {
            set(name, default, &mut changes);
        }
    }
    let applicable = blockers.is_empty();
    let proposed_body = encode(&proposed);
    let preview_id = sha256_hex(
        &[
            form.fingerprint().as_bytes(),
            b"|",
            backup_sha256.as_bytes(),
            b"|",
            proposed_body.as_bytes(),
        ]
        .concat(),
    );
    let preserved_controls = form
        .fields
        .iter()
        .filter(|(n, _)| !changes.iter().any(|c| &c.attribute == n))
        .count();
    (
        HqpSetupPreview {
            preview_id,
            applicable,
            blocker: (!applicable).then(|| blockers.join("; ")),
            current,
            changes: if applicable { changes } else { vec![] },
            preserved_controls,
            relay_option: relay_found.then_some(wanted),
            backup_sha256,
            proposed_sha256: sha256_hex(proposed_body.as_bytes()),
        },
        proposed,
    )
}

/// What the persistent configuration says about the output, for disk-side readback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskOutput {
    pub output_type: Option<String>,
    pub network_address: Option<String>,
    pub network_device: Option<String>,
}

pub fn disk_output(backup: &[u8]) -> Option<DiskOutput> {
    let text = std::str::from_utf8(backup).ok()?;
    if text.contains("<!") {
        return None;
    }
    let doc = roxmltree::Document::parse(text).ok()?;
    let output = doc.descendants().find(|n| n.has_tag_name("output"));
    let network = doc.descendants().find(|n| n.has_tag_name("network"));
    Some(DiskOutput {
        output_type: output.and_then(|n| n.attribute("type").map(str::to_string)),
        network_address: network.and_then(|n| n.attribute("address").map(str::to_string)),
        network_device: network.and_then(|n| n.attribute("device").map(str::to_string)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FORM: &str = r#"<html><body><form method="post">
<input type="text" name="title" value="HQPlayerEmbedded"/>
<select name="backend"><option value="alsa" selected>ALSA</option><option value="network">Network Audio</option><option value="combo">Combo</option></select>
<select name="mode"><option value="auto">Auto</option><option value="pcm">PCM</option><option value="sdm" selected>SDM</option></select>
<select name="filter"><option value="40" selected>poly-sinc-gauss-hires-lp</option><option value="41">other</option></select>
<select name="alsa_device"><option value="hw:CARD=null">null</option></select>
<select name="net_device"><option value="Relay/hiphi:router">Relay: Relay</option><option value="rpi/hw:CARD=C20,DEV=0">rpi: usb</option></select>
<input type="number" name="net_bits" value="0"/>
<input type="number" name="net_period" value="0"/>
<input type="checkbox" name="dsd_6db" value="1" checked/>
<input type="checkbox" name="net_dop" value="1"/>
<input type="checkbox" name="log_enabled" value="1" checked/>
<input type="submit" value="Apply"/>
</form><form method="get"><input type="text" name="unrelated" value="x"/></form></body></html>"#;

    #[test]
    fn successful_controls_follow_the_form_submission_rules() {
        let form = parse_form(FORM).unwrap_or_else(|e| panic!("{e}"));
        let names: Vec<&str> = form.fields.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "title",
                "backend",
                "mode",
                "filter",
                "alsa_device",
                "net_device",
                "net_bits",
                "net_period",
                "dsd_6db",
                "log_enabled"
            ],
            "unchecked boxes, submit buttons and other forms never contribute"
        );
        assert_eq!(form.value("backend"), Some("alsa"));
        assert_eq!(form.value("mode"), Some("sdm"));
        assert_eq!(
            form.value("alsa_device"),
            Some("hw:CARD=null"),
            "first option when none is selected"
        );
        assert_eq!(form.value("net_device"), Some("Relay/hiphi:router"));
        assert_eq!(form.options("net_device").map(|o| o.len()), Some(2));
    }

    #[test]
    fn derive_changes_only_the_output_selection_and_preserves_dsp_controls() {
        let form = parse_form(FORM).unwrap_or_else(|e| panic!("{e}"));
        let (preview, proposed) = derive(&form, b"<hqplayerd/>", "Relay", "hiphi:router");
        assert!(preview.applicable, "{preview:?}");
        assert_eq!(preview.relay_option.as_deref(), Some("Relay/hiphi:router"));
        let changed: Vec<(&str, &str)> = preview
            .changes
            .iter()
            .map(|c| (c.attribute.as_str(), c.to.as_str()))
            .collect();
        assert_eq!(
            changed,
            vec![
                ("backend", "network"),
                ("net_bits", "32"),
                ("net_period", "250")
            ],
            "net_device already names the relay, DSP untouched"
        );
        let posted: std::collections::HashMap<_, _> = proposed.iter().cloned().collect();
        assert_eq!(posted["mode"], "sdm");
        assert_eq!(posted["filter"], "40");
        assert_eq!(posted["dsd_6db"], "1");
        assert_eq!(posted["log_enabled"], "1");
        assert!(
            !posted.contains_key("net_dop"),
            "an unchecked box stays absent"
        );
        assert_eq!(preview.preserved_controls, form.fields.len() - 3);
        assert_eq!(
            preview
                .current
                .iter()
                .map(|a| a.name.as_str())
                .collect::<Vec<_>>(),
            vec![
                "backend",
                "net_device",
                "alsa_device",
                "net_bits",
                "net_period",
                "mode"
            ]
        );
        // Deterministic identity, sensitive to the form and to the backup bytes.
        let (again, _) = derive(&form, b"<hqplayerd/>", "Relay", "hiphi:router");
        assert_eq!(again.preview_id, preview.preview_id);
        let (other, _) = derive(&form, b"<hqplayerd changed='1'/>", "Relay", "hiphi:router");
        assert_ne!(other.preview_id, preview.preview_id);
    }

    #[test]
    fn an_undiscovered_relay_is_a_blocker_never_a_guess() {
        let form = parse_form(FORM).unwrap_or_else(|e| panic!("{e}"));
        let (preview, proposed) = derive(&form, b"<hqplayerd/>", "Living Room", "hiphi:router");
        assert!(!preview.applicable);
        assert!(preview
            .blocker
            .as_deref()
            .is_some_and(|b| b.contains("not discovered")));
        assert!(preview.changes.is_empty());
        assert_eq!(preview.relay_option, None);
        assert_eq!(
            proposed
                .iter()
                .find(|(n, _)| n == "net_device")
                .map(|(_, v)| v.as_str()),
            Some("Living Room/hiphi:router"),
            "the proposal is computed but never applicable"
        );
    }

    #[test]
    fn disk_output_reads_the_persistent_identity() {
        let disk = disk_output(b"<?xml version=\"1.0\"?><hqplayerd><output type=\"network\"/><network address=\"Relay\" device=\"hiphi:router\" friendly_name=\"Relay: Relay\"/></hqplayerd>")
            .unwrap_or_else(|| panic!("parses"));
        assert_eq!(disk.output_type.as_deref(), Some("network"));
        assert_eq!(disk.network_address.as_deref(), Some("Relay"));
        assert_eq!(disk.network_device.as_deref(), Some("hiphi:router"));
        assert!(disk_output(b"<!DOCTYPE x><hqplayerd/>").is_none());
    }
}
