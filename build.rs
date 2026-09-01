//! Build script to inject version and git SHA at compile time.
//!
//! Environment variables (set by CI or fall back to defaults):
//! - UHC_VERSION: Version string (defaults to CARGO_PKG_VERSION)
//! - UHC_GIT_SHA: Git commit SHA (defaults to "unknown" or git rev-parse)

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
