use std::fs;

const BYLINE: &str = "Unified Hi-Fi Control by Open Horizon Labs";

fn read(path: &str) -> String {
    fs::read_to_string(path).unwrap_or_else(|error| panic!("failed to read {path}: {error}"))
}

#[test]
fn public_entry_points_use_the_canonical_product_byline() {
    for path in [
        "README.md",
        "Dioxus.toml",
        "lms-plugin/strings.txt",
        "lms-plugin/repo.xml",
        "build/windows/installer.wxs",
        "build/macos/distribution.xml",
        "build/qnap/qpkg.cfg",
        "build/synology/INFO",
    ] {
        assert!(
            read(path).contains(BYLINE),
            "{path} must use the canonical public byline"
        );
    }
}

#[test]
fn controller_ui_uses_every_proper_product_name() {
    let page = read("src/app/pages/knobs.rs");

    for product in [
        "HiPhi Dial",
        "HiPhi Frame",
        "HiPhi RLCD",
        "HiPhi Joy",
        "HiPhi Tough",
        "M5 Dial Lab",
        "StickS3 Twist",
        "StopWatch Remote",
        "Kizz",
    ] {
        assert!(
            page.contains(product),
            "missing controller product name: {product}"
        );
    }
}

#[test]
fn compatibility_identifiers_are_not_rebranded() {
    assert!(read("Cargo.toml").contains("name = \"unified-hifi-control\""));
    assert!(read("Dioxus.toml").contains("name = \"unified-hifi-control\""));
    assert!(read("src/main.rs").contains(".route(\"/knob/devices\""));
    assert!(read("src/app/mod.rs").contains("\"unified-hifi-control\""));
}
