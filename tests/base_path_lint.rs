//! Every browser-issued URL in the client must flow through
//! `base_path::href` (#581).
//!
//! Behind Home Assistant Ingress the UI is served under
//! `/api/hassio_ingress/<token>/`. The server keeps absolute routes, so any
//! origin-absolute URL the *browser* issues has to be mapped onto that
//! prefix or it escapes the proxy and dies against Home Assistant itself.
//! `fetch_json`/`post_json` do this centrally, but `<img src>` attributes
//! are built ad hoc in each page -- and four of them were missed, so album
//! art was broken on Zones, HQPlayer, Spotify and the zones-strip picker
//! while every other surface worked.
//!
//! This lint reads the rsx source and, for each `src: "{expr}"`, requires
//! `expr` to be produced by a mapping helper: either inline, or via a
//! `let expr = ...` in the same file whose right-hand side calls one.
//! Coarse by design -- it does not understand Rust -- but it catches the
//! failure mode that actually happened: forgetting entirely.

use std::path::{Path, PathBuf};

/// Helpers that are known to apply the base path.
const MAPPERS: [&str; 2] = ["href(", "image_src("];

/// Escape hatch for the cases this text-level check cannot see: a `src` that
/// needs no mapping at all (a `data:` URL), or one whose value was mapped
/// where the struct carrying it was built. Put it on the `src` line or the
/// line above, and say why -- an unexplained marker is a review smell.
const ALLOW_MARKER: &str = "base-path-ok:";

fn client_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            client_sources(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// Pull the interpolated expression out of every `src: "{...}"` in `source`,
/// along with the 1-based line it sits on.
fn src_interpolations(source: &str) -> Vec<(usize, String)> {
    let lines: Vec<&str> = source.lines().collect();
    let mut found = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let Some(rest) = line.split_once("src: \"{") else {
            continue;
        };
        let Some((expr, _)) = rest.1.split_once('}') else {
            continue;
        };
        let allowed_here = line.contains(ALLOW_MARKER);
        let allowed_above = index
            .checked_sub(1)
            .is_some_and(|prev| lines[prev].contains(ALLOW_MARKER));
        if allowed_here || allowed_above {
            continue;
        }
        found.push((index + 1, expr.trim().to_string()));
    }
    found
}

/// Whether `expr` is mapped: inline, or by the `let` that defines it.
fn is_mapped(expr: &str, source: &str) -> bool {
    if MAPPERS.iter().any(|mapper| expr.contains(mapper)) {
        return true;
    }
    // `expr` may be a field access (`np.image_url`); the binding to look for
    // is the root identifier.
    let binding = expr.split(['.', ' ', '(']).next().unwrap_or(expr);
    if binding.is_empty() {
        return false;
    }
    // The LAST `let <binding> =` wins: pages deliberately shadow the raw
    // value with a mapped one (`let image_url = href(&image_url);`).
    let needle = format!("let {binding} =");
    let Some(start) = source.rfind(&needle) else {
        return false;
    };
    let tail = &source[start..];
    let statement = tail.find(";\n").map_or(tail, |end| &tail[..end]);
    MAPPERS.iter().any(|mapper| statement.contains(mapper))
}

#[test]
fn every_img_src_flows_through_the_base_path_resolver() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/app");
    let mut files = Vec::new();
    client_sources(&root, &mut files);
    assert!(!files.is_empty(), "no client sources found under src/app");

    let mut offenders = Vec::new();
    for file in files {
        let Ok(source) = std::fs::read_to_string(&file) else {
            continue;
        };
        for (line, expr) in src_interpolations(&source) {
            if !is_mapped(&expr, &source) {
                let shown = file
                    .strip_prefix(env!("CARGO_MANIFEST_DIR"))
                    .unwrap_or(&file)
                    .display();
                offenders.push(format!("{shown}:{line}: src: \"{{{expr}}}\""));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "these `<img src>` values never pass through `base_path::href` (or \
         `image_src`), so they break behind a Home Assistant Ingress prefix \
         (#581). Map the URL where it is built:\n  {}",
        offenders.join("\n  ")
    );
}

#[test]
fn the_lint_recognizes_both_mapped_and_unmapped_shapes() {
    // Unmapped: a plain binding built by `format!`.
    let unmapped = "let image_url = format!(\"{}k={}\", a, b);\n\
                    img { src: \"{image_url}\" }\n";
    assert_eq!(
        src_interpolations(unmapped)
            .into_iter()
            .filter(|(_, expr)| !is_mapped(expr, unmapped))
            .count(),
        1,
        "an unmapped src must be reported"
    );

    // Mapped inline, mapped via a shadowing `let`, and mapped through a
    // field-access binding.
    for mapped in [
        "img { src: \"{image_src(&image)}\" }\n",
        "let image_url = format!(\"x\");\nlet image_url = base_path::href(&image_url);\n\
         img { src: \"{image_url}\" }\n",
        "let np = base_path::href(&raw);\nimg { src: \"{np.image_url}\" }\n",
    ] {
        assert!(
            src_interpolations(mapped)
                .into_iter()
                .all(|(_, expr)| is_mapped(&expr, mapped)),
            "mapped src wrongly reported: {mapped}"
        );
    }
}
