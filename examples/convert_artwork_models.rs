//! Offline conversion only. Never included in a shipping binary.
//! cargo run --release --example convert-artwork-models -- /path/to/pinned/zensally
use sha2::{Digest, Sha256};
use std::{fs::File, io::Read, path::PathBuf};
use tract_onnx::prelude::*;

fn main() -> anyhow::Result<()> {
    let source = PathBuf::from(std::env::args_os().nth(1).ok_or_else(|| {
        anyhow::anyhow!("pass the zensally checkout at a8888d08c6d4bd1fe0969cdce40da6c01b8032be")
    })?);
    let output = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("crates/artwork-inference/models");
    for (input, name, h, w, expected_sha256) in [
        (
            "ultraface-rfb-320",
            "ultraface",
            240usize,
            320usize,
            "3421320bdcaaeb83ae9a0583f525194299b6976560cde12cb92f62b347f79ed1",
        ),
        (
            "microsalnet",
            "microsalnet",
            256usize,
            256usize,
            "0d8a0c14027d613fb7ff096851fc165128cd0228314672d2dd572468aa729a63",
        ),
    ] {
        let path = source
            .join("crates/zensally-tract/models")
            .join(format!("{input}.onnx.gz"));
        let mut bytes = Vec::new();
        let compressed = std::fs::read(&path)?;
        anyhow::ensure!(
            hex::encode(Sha256::digest(&compressed)) == expected_sha256,
            "{} differs from the pinned model; refusing conversion",
            path.display()
        );
        flate2::read::GzDecoder::new(compressed.as_slice()).read_to_end(&mut bytes)?;
        let model = tract_onnx::onnx()
            .model_for_read(&mut std::io::Cursor::new(bytes))?
            .with_input_fact(0, f32::fact([1, 3, h, w]).into())?
            .into_typed()?
            .into_decluttered()?;
        // Do not serialize target-optimized kernels: installers run on other CPUs.
        let file = File::create(output.join(format!("{name}.nnef.tar.gz")))?;
        let gzip = flate2::GzBuilder::new()
            .mtime(0)
            .write(file, flate2::Compression::best());
        tract_nnef::nnef()
            .write_to_tar_with_config(&model, gzip, false, true)?
            .finish()?;
    }
    Ok(())
}
