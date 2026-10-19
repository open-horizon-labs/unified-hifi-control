//! Composition belongs to shared zone control, not to a particular client.
#[test]
fn surfaces_do_not_resolve_zone_device_links() {
    let cloud = include_str!("../src/cloud_connector/runtime.rs");
    assert!(
        !cloud.contains("linked_device_id("),
        "HiPhi must delegate capability routing to shared zone control"
    );
    let knob = include_str!("../src/knobs/routes.rs");
    assert!(
        !knob.contains("linked_device_id("),
        "knob handlers must not implement their own capability routing"
    );
    let mcp = include_str!("../src/mcp/tools/devices.rs");
    let linked = mcp
        .split("pub async fn linked_zone_volume(")
        .nth(1)
        .unwrap();
    assert!(
        !linked.contains("linked_device_id("),
        "MCP zone control must delegate link resolution"
    );
}

#[test]
fn device_service_owns_native_action_interpretation() {
    let router = include_str!("../src/zone_control.rs");
    for scattered_policy in [
        "request_from_action(",
        ".volume.step",
        "VolumeScale::",
        "linked_device_id(",
    ] {
        assert!(
            !router.contains(scattered_policy),
            "shared router must delegate device interpretation: {scattered_policy}"
        );
    }
    for (surface, text) in [
        ("HTTP device control", include_str!("../src/api/devices.rs")),
        (
            "MCP device control",
            include_str!("../src/mcp/tools/devices.rs"),
        ),
    ] {
        assert!(
            !text.contains("request_from_action("),
            "{surface} must delegate native defaults and action parsing"
        );
        assert!(
            !text.contains("linked_device_id("),
            "{surface} must delegate composition lookup"
        );
    }
}

#[test]
fn shared_policy_does_not_depend_on_client_implementations() {
    let router = include_str!("../src/zone_control.rs");
    assert!(!router.contains("crate::knobs::routes"));
    let provider = include_str!("../src/provider_control.rs");
    assert!(!provider.contains("use crate::api"));
    assert!(!provider.contains("use crate::knobs"));
    let mqtt = include_str!("../src/mqtt/state.rs")
        .split("#[cfg(test)]")
        .next()
        .unwrap();
    assert!(
        !mqtt.contains("VolumeScale::"),
        "client percentage conversion belongs to the shared volume model"
    );
    let main = include_str!("../src/main.rs");
    assert!(
        !main.contains("document.manual_hosts"),
        "discovery config interpretation belongs to its configuration owner"
    );
}
