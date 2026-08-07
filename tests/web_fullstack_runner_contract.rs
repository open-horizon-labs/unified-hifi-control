//! Regression contract for the local interactive-web runner.
//!
//! Dioxus Settings is hydrated by a browser WASM bundle. Compiling only the
//! Rust server (`cargo run`) can therefore serve new SSR markup with old client
//! code, which breaks otherwise ordinary interactions such as feature
//! switches. `make web-run` is the supported path: it must build CSS and the
//! Dioxus fullstack bundle, then execute *that exact build's* server. Keep this
//! contract in sync if the build layout changes; do not weaken it to accept a
//! standalone cargo server.

const MAKEFILE: &str = include_str!("../Makefile");
const WEB_RUNNER: &str = include_str!("../scripts/run-web.sh");
const README: &str = include_str!("../README.md");

#[test]
fn make_web_run_is_the_single_fullstack_entrypoint() {
    let target = MAKEFILE
        .split_once("web-run: web-prereqs\n")
        .expect("Makefile must define the supported web-run target")
        .1
        .lines()
        .next()
        .expect("web-run must invoke the deterministic fullstack runner");

    assert!(
        target.contains("./scripts/run-web.sh"),
        "make web-run must delegate to scripts/run-web.sh rather than duplicating a partial build"
    );
    assert!(
        README.contains("Never use `cargo run` for the web UI"),
        "the development instructions must warn that cargo run cannot refresh the hydrated WASM client"
    );
}

#[test]
fn web_run_requires_the_wasm_target_and_uses_rustup_toolchain() {
    assert!(
        MAKEFILE.contains("web-run: web-prereqs"),
        "web-run must verify its Rust/WASM prerequisites before building"
    );
    assert!(
        MAKEFILE.contains("rustup target add wasm32-unknown-unknown"),
        "web-prereqs must install the WASM target through rustup"
    );
    assert!(
        MAKEFILE.contains("rustup which cargo"),
        "web-run must put the active rustup toolchain ahead of another cargo on PATH"
    );
}

#[test]
fn runner_builds_matching_assets_before_it_starts_the_server() {
    let css = WEB_RUNNER
        .find("make css")
        .expect("the runner must build the stylesheet");
    let wasm = WEB_RUNNER
        .find("dx build --release --platform web --features web")
        .expect("the runner must build the Dioxus WASM client and server together");
    let server = WEB_RUNNER
        .find("target/dx/unified-hifi-control/release/web/server")
        .expect("the runner must launch the server emitted by dx build");
    let exec = WEB_RUNNER
        .find("exec env PORT=")
        .expect("the runner must replace itself with the freshly built server");

    assert!(
        css < wasm && wasm < server && server < exec,
        "build CSS and the fullstack Dioxus bundle before launching its generated server"
    );
    assert!(
        !WEB_RUNNER.lines().map(str::trim_start).any(|line| {
            line.starts_with("cargo run") || line.starts_with("exec cargo run")
        }),
        "cargo run produces a server without a guaranteed matching browser bundle; use dx build instead"
    );
}
