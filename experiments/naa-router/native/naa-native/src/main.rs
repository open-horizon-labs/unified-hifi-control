use naa_native::{ParseError, Parser, Record};
use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::PathBuf,
    process,
};
fn usage() -> ! {
    eprintln!(
        "usage: naa-replay --sample-bytes N --input PATH [--audio-out PATH] [--control-out PATH] [--chunk-size N]"
    );
    process::exit(2)
}
fn main() {
    if env::args().any(|a| a == "--server") {
        server_main();
        return;
    }
    let mut sample = None;
    let mut input = None;
    let mut audio = None;
    let mut control = None;
    let mut chunk = 8192usize;
    let a: Vec<String> = env::args().skip(1).collect();
    let mut i = 0;
    while i < a.len() {
        match a[i].as_str() {
            "--sample-bytes" => {
                i += 1;
                sample = a.get(i).and_then(|v| v.parse().ok())
            }
            "--input" => {
                i += 1;
                input = a.get(i).map(PathBuf::from)
            }
            "--audio-out" => {
                i += 1;
                audio = a.get(i).map(PathBuf::from)
            }
            "--control-out" => {
                i += 1;
                control = a.get(i).map(PathBuf::from)
            }
            "--chunk-size" => {
                i += 1;
                chunk = a.get(i).and_then(|v| v.parse().ok()).unwrap_or(0)
            }
            _ => usage(),
        }
        i += 1
    }
    let sample = sample.unwrap_or_else(|| usage());
    let input = input.unwrap_or_else(|| usage());
    if chunk == 0 {
        usage()
    }
    if chunk > 1024 * 1024 {
        eprintln!("chunk size must be at most 1048576 bytes");
        process::exit(2)
    }
    let mut p = Parser::new(sample, 64 * 1024 * 1024, 1024 * 1024).unwrap_or_else(|e| {
        eprintln!("{e}");
        process::exit(2)
    });
    let mut f = File::open(&input).unwrap_or_else(|e| {
        eprintln!("input: {e}");
        process::exit(1)
    });
    let mut out = match audio {
        Some(p) => Some(open_output(&p, &input)),
        None => None,
    };
    let mut control_out = control.map(|p| open_output(&p, &input));
    let mut buf = vec![0; chunk];
    loop {
        let n = match f.read(&mut buf) {
            Ok(n) => n,
            Err(e) => {
                eprintln!("read: {e}");
                process::exit(1)
            }
        };
        if n == 0 {
            break;
        }
        if let Err(e) = p.feed(&buf[..n], |r| emit(r, &mut out, &mut control_out)) {
            emit_error(e);
            process::exit(1)
        }
    }
    if let Err(e) = p.finish(|r| emit(r, &mut out, &mut control_out)) {
        emit_error(e);
        process::exit(1)
    }
}
fn server_main() {
    if let Err(e) = naa_native::server::run() {
        eprintln!("NAA server: {e}");
        process::exit(1)
    }
}

fn open_output(path: &PathBuf, input: &PathBuf) -> File {
    let im = fs::metadata(input).unwrap_or_else(|e| {
        eprintln!("input metadata: {e}");
        process::exit(1)
    });
    if let Ok(om) = fs::metadata(path) {
        if om.dev() == im.dev() && om.ino() == im.ino() {
            eprintln!("audio output is the input file or hardlink");
            process::exit(2)
        }
    }
    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let parent = fs::canonicalize(parent).unwrap_or_else(|e| {
        eprintln!("audio output parent: {e}");
        process::exit(2)
    });
    if let Ok(root) = process::Command::new("git")
        .args([
            "-C",
            &parent.to_string_lossy(),
            "rev-parse",
            "--show-toplevel",
        ])
        .output()
    {
        if root.status.success() {
            let r = fs::canonicalize(String::from_utf8_lossy(&root.stdout).trim()).unwrap_or_else(
                |e| {
                    eprintln!("git worktree path: {e}");
                    process::exit(2)
                },
            );
            if parent == r || parent.starts_with(&r) {
                eprintln!("audio output must be outside the git worktree");
                process::exit(2)
            }
        }
    }
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap_or_else(|e| {
            eprintln!("audio output: {e}");
            process::exit(1)
        })
}
fn emit(r: Record, out: &mut Option<File>, control_out: &mut Option<File>) {
    match r {
        Record::Control { offset, bytes } => {
            if let Some(f) = control_out {
                if let Err(e) = f.write_all(&bytes) {
                    eprintln!("control output: {e}");
                    process::exit(1)
                }
            }
            println!(
                r#"{{"type":"control","offset":{},"bytes":{}}}"#,
                offset,
                bytes.len()
            )
        }
        Record::Audio(a) => {
            if let Some(f) = out {
                if let Err(e) = f.write_all(&a.pcm) {
                    eprintln!("audio output: {e}");
                    process::exit(1)
                }
            }
            println!(
                r#"{{"type":"audio","offset":{},"pcm_bytes":{},"position_bytes":{},"metadata_bytes":{},"picture_bytes":{},"mask":{},"framing_profile":"provisional32"}}"#,
                a.offset,
                a.pcm.len(),
                a.position.len(),
                a.metadata.len(),
                a.picture.len(),
                a.header.mask
            )
        }
    }
}
fn emit_error(e: ParseError) {
    println!(
        r#"{{"type":"error","offset":{},"kind":"{:?}"}}"#,
        e.offset, e.kind
    )
}
