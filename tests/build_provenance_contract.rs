//! Static regression guard for truthful `/status` git-sha provenance (#572).
//!
//! `build.rs` bakes `UHC_GIT_SHA` into the binary via `env!("UHC_GIT_SHA")`
//! (see `src/api/mod.rs::status_handler`). When neither `UHC_GIT_SHA` nor
//! `GITHUB_SHA` is set -- the common local `make web-run` path -- the SHA
//! comes from a `git rev-parse` shelled out inside `build.rs`. Cargo only
//! reruns a build script when a declared file/env dependency changes; the
//! original version declared none tied to git state, so after the first
//! local build `/status` kept reporting whatever commit was checked out at
//! that first build forever, surviving later commits, checkouts, and
//! rebases. That sent a live diagnosis chasing the wrong commit twice (see
//! the #572 issue history).
//!
//! Actually proving the fix requires a real double-build (see the PR
//! description for the empirical transcript: build, note `/status`'s sha,
//! make an empty commit, rebuild, confirm the new sha) -- that isn't
//! something a fast unit test can reproduce cheaply. This test instead pins
//! the regression at the source level, in the style of
//! `tests/web_fullstack_runner_contract.rs`: `build.rs` must still declare a
//! `cargo:rerun-if-changed` on the files that move whenever HEAD moves, so a
//! future edit can't silently drop the fix back to the frozen-SHA bug.
use std::fs;

#[test]
fn build_script_reruns_when_head_moves() {
    let build_rs = fs::read_to_string("build.rs").expect("read build.rs");

    assert!(
        build_rs.contains("cargo:rerun-if-changed"),
        "build.rs must declare cargo:rerun-if-changed dependencies so cargo reruns it when the \
         checked-out commit changes -- without this, UHC_GIT_SHA freezes at whatever commit was \
         checked out the first time this crate compiled (#572)"
    );
    assert!(
        build_rs.contains("--git-path"),
        "build.rs must resolve its git-state watch paths via `git rev-parse --git-path` so the \
         rerun trigger also works correctly from a linked worktree (where .git is a file, not a \
         directory, and HEAD lives under .git/worktrees/<name>/)"
    );
    assert!(
        build_rs.contains("\"HEAD\""),
        "build.rs must watch git's HEAD file so a new commit or checkout invalidates the cached SHA"
    );
    assert!(
        build_rs.contains("logs/HEAD"),
        "build.rs must watch git's HEAD reflog so commits, checkouts, and rebases all invalidate \
         the cached SHA (a plain HEAD watch alone misses cases where HEAD's target ref file moves \
         but HEAD's own symbolic-ref contents do not)"
    );

    // The env-var overrides (CI's explicit UHC_GIT_SHA / GITHUB_SHA) must
    // still take precedence and still be declared, unchanged from before.
    for env_var in ["UHC_GIT_SHA", "UHC_VERSION", "GITHUB_SHA"] {
        assert!(
            build_rs.contains(&format!("cargo:rerun-if-env-changed={env_var}")),
            "build.rs dropped its cargo:rerun-if-env-changed declaration for {env_var}"
        );
    }
}
