# Frame rendering reference (#906)

`input.rgb` contains 32×12 RGB888 synthetic pixels, generated as:
R=(19*x+7*y)%256, G=(3*x+23*y)%256, B=(11*x+13*y)%256.
`measured.epd` is the high-nibble-first stock-panel result from the independent
Python implementation used in muness/roon-knob#318 TEST 2. It exercises tone
compression, all palette colors and diffusion across rows without album art.

The Rust test was added first and failed against the old ideal-palette renderer.
The reference comes from the existing experiment, not from the Rust output.
The recipe uses the donor firmware device-style luminance CDR (4096-entry
linear-to-sRGB lookup), stock measured palette and float raster Floyd–Steinberg.
The donor companion CLI's Lab lightness compression is a different recipe.
