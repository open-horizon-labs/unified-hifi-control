#![forbid(unsafe_code)]
//! Same Zensally image preprocessing and output interpretation, with the ONNX
//! training-format importer excluded from the shipping executable. Portable,
//! decluttered NNEF graphs are optimized for the actual CPU when loaded.
mod analyzer;
mod microsalnet;
mod output;
mod ultraface;
pub use analyzer::ContentAnalyzer;

use std::io::Read;
use tract_nnef::prelude::*;

fn load_model(compressed: &[u8]) -> anyhow::Result<std::sync::Arc<TypedRunnableModel>> {
    let mut model = Vec::new();
    flate2::read::GzDecoder::new(compressed).read_to_end(&mut model)?;
    Ok(tract_nnef::nnef()
        .model_for_read(&mut std::io::Cursor::new(model))?
        .into_optimized()?
        .into_runnable()?)
}
