# HiPhi Frame artwork rendering

Frame receives packed `eink_acep6`: two hardware indices per byte, left pixel in
the high nibble. The six valid codes are 0, 1, 2, 3, 5 and 6. There is no old
ideal-palette renderer or renderer selector.

UHC retains its existing decode, composition and linear-light zenresize pipeline
(#802): Robidoux for reductions, Ginseng with mild sharpening for enlargements.
Only the following Frame-specific encoding stage changes:

1. Compress source linear luminance into the stock panel black/white range,
   scaling RGB proportionally and clipping to the source gamut.
2. Match colors using the stock measured RGB values below.
3. Diffuse residual error against those measured values using float
   Floyd–Steinberg accumulators. Clamp the working pixel once before matching,
   retaining fractional contributions between pixels/rows.
4. Emit the same hardware indices and byte lengths as before.

| Code | Color | Measured RGB |
|---|---|---|
| 0 | Black | 2, 2, 2 |
| 1 | White | 190, 200, 200 |
| 2 | Yellow | 205, 202, 0 |
| 3 | Red | 135, 19, 0 |
| 5 | Blue | 5, 64, 158 |
| 6 | Green | 39, 102, 60 |

These are donor stock values, not per-unit measurements. Calibration was
explicitly excluded. E-ink output cache revision 5 invalidates previous encodings;
non-e-ink encodings and their revision are unchanged. Devices already holding an
unchanged image need a normal artwork change/reconnect before fetching new bytes.

The independent TEST 2 experiment on the maintainer's PhotoPainter was preferred
on physical hardware, after recovering a separate PMIC/panel freeze. Both sides
used the same composed JPEG input; the test did not establish the exact version
of the live resizer. The production replacement has a separate exact-build
validation requirement. See muness/roon-knob#318 and #320, and UHC #906.

Behavior and palette provenance: aitjcize/esp32-photoframe commit
04993f686c541a0afed030313e227d50928842ae, main/image_processor.c and
main/color_palette.c. This Rust implementation is independently written. The
donor's companion converter uses a different Lab-lightness compression; it was
not substituted for the device-style recipe preferred in the experiment.
