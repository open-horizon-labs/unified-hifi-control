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
