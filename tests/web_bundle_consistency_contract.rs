//! Guard against serving mismatched client bundles (issue #572).
//!
//! #566's postmortem (see `src/embedded.rs::disk_public_root`) found that an
//! SSR page could execute a stale WASM client because more than one build's
//! worth of `assets/*.js` / `*.wasm` bundles were in play at once, or the
//! served `index.html` pointed at a bundle that no longer matched what was
//! on disk. That defect was fixed by preferring the fresh `dx build` output
//! next to the running binary, but nothing asserted the invariant directly:
//! the served SSR shell must reference **exactly one** client JS bundle and
//! **exactly one** wasm bundle, and both files must actually exist in the
//! served asset set.
//!
//! This is an integration test against `dx`'s *built output*
//! (`target/dx/unified-hifi-control/release/web/public/`), the exact
//! directory [`crate::embedded::disk_public_root`] serves from at runtime.
//! That output only exists after `make web-run` (or CI's equivalent `dx
//! build`) has run, so — in the style of
//! `tests/web_fullstack_runner_contract.rs`, which pins build-layout
//! invariants from the repo's static files — the checks that don't need a
//! build run unconditionally, and the on-disk bundle assertion runs fully
//! whenever dx output is present and skips loudly (not silently) otherwise.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Where `dx build --platform web` (see `scripts/run-web.sh`) writes the
/// fullstack client's static assets, next to the server binary it built.
fn dx_public_dir() -> PathBuf {
    Path::new("target/dx/unified-hifi-control/release/web/public").to_path_buf()
}

/// Every `src="..."` attribute value found in `html`, in document order.
/// Good enough for the small, machine-generated `index.html` dx emits; this
/// test only needs to find the module-script reference, not parse HTML.
fn extract_attr_values(html: &str, attr: &str) -> Vec<String> {
    let needle = format!("{attr}=\"");
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(start) = rest.find(&needle) {
        rest = &rest[start + needle.len()..];
        if let Some(end) = rest.find('"') {
            out.push(rest[..end].to_string());
            rest = &rest[end + 1..];
        } else {
            break;
        }
    }
    out
}

/// Resolve an `index.html` asset reference (e.g. `/./assets/foo-HASH.js`)
/// to a path relative to the public dir.
fn relative_asset_path(reference: &str) -> Option<String> {
    let trimmed = reference
        .trim_start_matches('/')
        .trim_start_matches("./")
        .trim_start_matches('/');
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[test]
fn dx_public_dir_path_matches_what_the_server_serves_from() {
    // Keep this test honest against src/embedded.rs: if the disk-serving
    // path ever moves, this test's target directory must move with it.
    let embedded_src = fs::read_to_string("src/embedded.rs").expect("read src/embedded.rs");
    assert!(
        embedded_src.contains(r#"exe.parent()?.join("public")"#),
        "src/embedded.rs must still serve the built client from next to the running binary; \
         update dx_public_dir() in this test if that layout changes"
    );
    assert!(
        embedded_src
            .contains(r#"#[folder = "target/dx/unified-hifi-control/release/web/public/"]"#),
        "src/embedded.rs's embedded snapshot folder moved; update dx_public_dir() in this test"
    );
}

/// The bundle-consistency assertion this issue asks for. Runs fully when
/// `make web-run` / CI's `dx build` has already produced output in this
/// checkout; otherwise skips loudly rather than silently passing or failing
/// a machine that never ran a web build.
///
/// dx's SSR shell (`index.html`) references the client JS entrypoint
/// directly via `<script type="module" src="...">`; the paired `.wasm`
/// module is not linked from the HTML at all -- wasm-bindgen's glue code
/// fetches it by its hashed filename from inside the JS bundle. So "exactly
/// one JS bundle + one wasm, with matching hashes" is checked in two hops:
/// (1) the shell must name exactly one JS entrypoint and that file must
/// exist, and (2) the served asset set must contain exactly one `.wasm`
/// file, and that JS entrypoint's own source must reference it by its exact
/// (content-hashed) filename -- i.e. the JS and wasm the server is actually
/// serving are the ones built together, not a stale pairing.
#[test]
fn ssr_shell_references_exactly_one_js_bundle_and_one_wasm_with_matching_files_on_disk() {
    let public_dir = dx_public_dir();
    let index_path = public_dir.join("index.html");

    if !index_path.is_file() {
        eprintln!(
            "SKIPPING ssr_shell_references_exactly_one_js_bundle_and_one_wasm_with_matching_files_on_disk: \
             no dx build output at {} (run `make web-run` or CI's dx build first to exercise this guard)",
            index_path.display()
        );
        return;
    }

    let html = fs::read_to_string(&index_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", index_path.display()));

    // (1) Exactly one client JS bundle referenced by the shell.
    let js_refs: BTreeSet<String> = extract_attr_values(&html, "src")
        .into_iter()
        .filter(|src| src.ends_with(".js"))
        .collect();
    assert_eq!(
        js_refs.len(),
        1,
        "the SSR shell at {} must reference exactly one client JS bundle via <script src>, found {:?} \
         (a second bundle reference is how #566's mount hang happened -- see needs_bootstrap in src/embedded.rs)",
        index_path.display(),
        js_refs
    );
    let js_ref = js_refs.into_iter().next().unwrap();
    let js_rel = relative_asset_path(&js_ref)
        .unwrap_or_else(|| panic!("could not resolve JS reference {js_ref:?} to a relative path"));
    let js_path = public_dir.join(&js_rel);
    assert!(
        js_path.is_file(),
        "the SSR shell references JS bundle {js_ref:?} but no such file exists at {} -- \
         the served bundle set is stale relative to what the shell advertises",
        js_path.display()
    );
    let js_bytes = fs::read(&js_path).unwrap_or_else(|e| panic!("read {}: {e}", js_path.display()));
    assert!(
        !js_bytes.is_empty(),
        "referenced JS bundle {} is empty",
        js_path.display()
    );
    let js_text = String::from_utf8_lossy(&js_bytes);

    // (2) Exactly one wasm bundle in the served asset set, and the JS
    // entrypoint names it exactly (the hash match).
    let assets_dir = public_dir.join("assets");
    let wasm_files: Vec<String> = fs::read_dir(&assets_dir)
        .unwrap_or_else(|e| panic!("read_dir {}: {e}", assets_dir.display()))
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name.ends_with(".wasm").then_some(name)
        })
        .collect();
    assert_eq!(
        wasm_files.len(),
        1,
        "the served asset set at {} must contain exactly one wasm bundle, found {:?}",
        assets_dir.display(),
        wasm_files
    );
    let wasm_file = &wasm_files[0];
    let wasm_path = assets_dir.join(wasm_file);
    let wasm_bytes =
        fs::read(&wasm_path).unwrap_or_else(|e| panic!("read {}: {e}", wasm_path.display()));
    assert!(
        !wasm_bytes.is_empty(),
        "wasm bundle {} is empty",
        wasm_path.display()
    );

    assert!(
        js_text.contains(wasm_file.as_str()),
        "client JS bundle {} does not reference the wasm bundle {wasm_file:?} that is actually on disk \
         (found no occurrence of its exact hashed filename) -- the JS/wasm pair being served is mismatched",
        js_path.display()
    );

    eprintln!(
        "verified dx build output: SSR shell references exactly one JS bundle ({js_rel:?}), \
         exactly one wasm bundle ({wasm_file:?}) exists on disk, and the JS bundle names it by its exact hash"
    );
}
