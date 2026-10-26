// Derived from Imazen zensally-tract at a8888d08c6d4bd1fe0969cdce40da6c01b8032be.
// SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-Imazen-Commercial
//! Model-output accessors shared by every detector.

use tract_nnef::prelude::*;

/// Borrow a model output as a contiguous `f32` slice.
///
/// tract 0.23 moved slice access off `Tensor` onto views: a tensor must be
/// verified as plain (CPU-resident, contiguous) storage before its bytes can
/// be read. Every model here runs on the CPU, so a non-plain output is a
/// contract violation reported as an error, never a panic.
pub(crate) fn plain_f32(tensor: &Tensor) -> TractResult<&[f32]> {
    tensor
        .as_plain()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "model output is not a plain tensor (dt={:?}, shape={:?})",
                tensor.datum_type(),
                tensor.shape()
            )
        })?
        .as_slice::<f32>()
}
