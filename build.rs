//! Build script to inject version and git SHA at compile time.
//!
//! Environment variables (set by CI or fall back to defaults):
//! - UHC_VERSION: Version string (defaults to CARGO_PKG_VERSION)
//! - UHC_GIT_SHA: Git commit SHA (defaults to "unknown" or git rev-parse)
//!
//! #572: when neither env var is set (the common local `make web-run` path),
//! the SHA is computed once by shelling out to `git rev-parse`. Cargo only
//! reruns a build script when a file/env dependency it declared changes, and
//! the old version declared none tied to git state — so after the first
//! build, `UHC_GIT_SHA` was frozen at whatever commit happened to be checked
//! out the first time, and every later rebuild (new commits, checkouts,
//! rebases) kept serving that stale SHA from `/status` even though nothing
//! else about the build was cached. That sent a live diagnosis session
//! chasing the wrong commit twice. Declaring `cargo:rerun-if-changed` on the
//! files that move whenever HEAD moves closes the gap.

use std::process::Command;

fn main() {
    // Version: prefer UHC_VERSION env var, fall back to CARGO_PKG_VERSION
    let version = std::env::var("UHC_VERSION").unwrap_or_else(|_| {
        std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "unknown".into())
    });
    println!("cargo:rustc-env=UHC_VERSION={}", version);

    // Git SHA: prefer UHC_GIT_SHA, then GITHUB_SHA, then try git command
    let git_sha = std::env::var("UHC_GIT_SHA")
        .or_else(|_| std::env::var("GITHUB_SHA").map(|s| s[..7].to_string()))
        .unwrap_or_else(|_| get_git_sha());
    println!("cargo:rustc-env=UHC_GIT_SHA={}", git_sha);

    // Rebuild if these change
    println!("cargo:rerun-if-env-changed=UHC_VERSION");
    println!("cargo:rerun-if-env-changed=UHC_GIT_SHA");
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");

    // Rebuild whenever the checked-out commit moves, so a plain rebuild
    // (with no env vars set) picks up the new SHA instead of the one that
    // happened to be current the first time this crate compiled. `--git-path`
    // resolves these correctly for linked worktrees too (where `.git` is a
    // file, not a directory, and HEAD lives under `.git/worktrees/<name>/`).
    for rel in ["HEAD", "logs/HEAD"] {
        if let Some(path) = git_path(rel) {
            println!("cargo:rerun-if-changed={}", path);
        }
    }
}

/// Resolve a path relative to the repo's (possibly worktree-specific) git
/// directory via `git rev-parse --git-path`, e.g. `HEAD` or `logs/HEAD`.
fn git_path(rel: &str) -> Option<String> {
    Command::new("git")
        .args(["rev-parse", "--git-path", rel])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                String::from_utf8(o.stdout)
                    .ok()
                    .map(|s| s.trim().to_string())
            } else {
                None
            }
        })
}

fn get_git_sha() -> String {
    Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                String::from_utf8(o.stdout)
                    .ok()
                    .map(|s| s.trim().to_string())
            } else {
                None
            }
        })
        .unwrap_or_else(|| "unknown".into())
}
