use tract_core::prelude::*;
#[cfg(feature = "onnx")]
use tract_onnx::prelude::InferenceModelExt;
#[cfg(feature = "onnx")]
fn onnx(path: &str, h: usize, w: usize) -> TractResult<TypedModel> {
    tract_onnx::onnx()
        .model_for_path(path)?
        .with_input_fact(0, f32::fact([1, 3, h, w]).into())?
        .into_typed()?
        .into_decluttered()
}
fn main() -> TractResult<()> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() == 1 {
        return Ok(());
    }
    let h: usize = args[3].parse()?;
    let w: usize = args[4].parse()?;
    #[cfg(feature = "convert")]
    if args[1] == "convert" {
        let writer = flate2::GzBuilder::new().mtime(0).write(
            std::fs::File::create(&args[5])?,
            flate2::Compression::best(),
        );
        tract_nnef::nnef()
            .write_to_tar_with_config(&onnx(&args[2], h, w)?, writer, false, true)?
            .finish()?;
        return Ok(());
    }
    #[cfg(feature = "onnx")]
    let model = onnx(&args[2], h, w)?;
    #[cfg(all(feature = "nnef", not(feature = "onnx")))]
    let model = tract_nnef::nnef().model_for_path(&args[2])?;
    #[cfg(any(feature = "onnx", feature = "nnef"))]
    {
        let plan = model.into_optimized()?.into_runnable()?;
        let input = tract_ndarray::Array4::from_shape_fn((1, 3, h, w), |(n, c, y, x)| {
            (((x * 13 + y * 7 + c * 31 + n) % 256) as f32) / 127.5 - 1.0
        })
        .into_tensor();
        let start = std::time::Instant::now();
        let outputs = plan.run(tvec!(input.into()))?;
        let mut bytes = vec![];
        for tensor in outputs {
            for value in tensor.to_plain_array_view::<f32>()?.iter() {
                bytes.extend(value.to_le_bytes());
            }
        }
        std::fs::write(&args[5], &bytes)?;
        println!(
            "output_bytes={} elapsed_ms={}",
            bytes.len(),
            start.elapsed().as_millis()
        );
    }
    Ok(())
}
