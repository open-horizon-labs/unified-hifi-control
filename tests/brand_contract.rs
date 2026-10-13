//! Product identity contract (#503): one canonical brand, Open Horizon Labs
//! attribution on package metadata, and no rename of stable machine identifiers.

use std::fs;

use unified_hifi_control::brand;

fn read(path: &str) -> String {
    fs::read_to_string(path).unwrap_or_else(|error| panic!("failed to read {path}: {error}"))
}

#[test]
fn canonical_brand_is_product_by_company() {
    assert_eq!(brand::PRODUCT_NAME, "Unified Hi-Fi Control");
    assert_eq!(brand::COMPANY_NAME, "Open Horizon Labs");
    assert_eq!(
        brand::PRODUCT_BYLINE,
        "Unified Hi-Fi Control by Open Horizon Labs"
    );
}

#[test]
fn package_metadata_is_attributed_to_the_company() {
    for (path, needle) in [
        ("Cargo.toml", "authors = [\"Open Horizon Labs\"]"),
        ("package.json", "\"author\": \"Open Horizon Labs\""),
        ("build/qnap/qpkg.cfg", "QPKG_AUTHOR=\"Open Horizon Labs\""),
        (
            "build/windows/installer.wxs",
            "ProductManufacturer = \"Open Horizon Labs\"",
        ),
        (
            "lms-plugin/install.xml",
            "<creator>Open Horizon Labs</creator>",
        ),
        (
            "lms-plugin/repo.xml",
            "<creator>Open Horizon Labs</creator>",
        ),
        (
            "lms-plugin/repo-beta.xml",
            "<creator>Open Horizon Labs</creator>",
        ),
    ] {
        assert!(read(path).contains(needle), "{path} must contain {needle}");
    }
}

#[test]
fn web_ui_takes_its_name_from_the_brand_module() {
    let layout = read("src/app/components/layout.rs");
    assert!(
        layout.contains("brand::PRODUCT_NAME") && layout.contains("brand::COMPANY_NAME"),
        "layout title and footer must use the canonical brand"
    );
}

#[test]
fn controllers_page_does_not_use_product_facing_knob_wording() {
    let page = read("src/app/pages/knobs.rs");
    for stale in [
        "Loading knobs...",
        "No knobs registered. Connect a knob",
        "Living Room Knob",
    ] {
        assert!(!page.contains(stale), "stale wording: {stale}");
    }
}

#[test]
fn compatibility_identifiers_are_not_rebranded() {
    assert!(read("Cargo.toml").contains("name = \"unified-hifi-control\""));
    assert!(read("Dioxus.toml").contains("name = \"unified-hifi-control\""));
    assert!(read("src/main.rs").contains("\"/knob/devices\""));
    assert!(read("src/mcp/server.rs").contains("name: \"unified-hifi-control\".into()"));
}
