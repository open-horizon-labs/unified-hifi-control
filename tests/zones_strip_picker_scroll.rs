//! Regression checks for the long player picker in the Bridge library UI.

fn picker_list_css() -> &'static str {
    let css = include_str!("../src/input.css");
    let start = css
        .find(".zones-strip-picker-list {")
        .expect("zones-strip-picker-list rule must exist");
    let rest = &css[start..];
    let end = rest.find('}').expect("picker list rule must be closed");
    &rest[..end]
}

#[test]
fn long_player_picker_owns_vertical_wheel_scrolling() {
    let rule = picker_list_css();

    assert!(
        rule.contains("max-height: min(50vh, calc(100dvh - 15rem), 24rem)"),
        "a long player picker must be bounded against both viewport sizes"
    );
    assert!(
        rule.contains("overflow-y: auto"),
        "the player picker itself must consume vertical wheel scrolling"
    );
    assert!(
        rule.contains("overscroll-behavior: contain"),
        "scroll chaining from the player picker to the page must be contained"
    );
}
