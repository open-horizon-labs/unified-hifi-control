/**
 * RGB565 conversion regression tests
 *
 * Bug: PNG images with alpha channels (RGBA, 4 bytes/pixel) caused channel
 * misalignment because the conversion loop assumed 3 bytes/pixel (RGB).
 *
 * Fix: Handle RGBA (4 bytes/pixel) from jimp bitmap data
 *
 * Issue: https://github.com/open-horizon-labs/unified-hifi-control/issues/35
 * Forum: https://forums.lyrion.org/forum/user-forums/3rd-party-hardware/1804977-roon-knob-includes-lms-support?p=1805839#post1805839
 */

const { Jimp } = require('jimp');

function convertToRgb565(rgba, width, height) {
  const rgb565 = Buffer.alloc(width * height * 2);
  for (let i = 0; i < rgba.length; i += 4) {
    const r = rgba[i] >> 3;
    const g = rgba[i + 1] >> 2;
    const b = rgba[i + 2] >> 3;
    // Skip alpha at rgba[i + 3]
    const rgb565Pixel = (r << 11) | (g << 5) | b;
    const pixelIndex = (i / 4) * 2;
    rgb565[pixelIndex] = rgb565Pixel & 0xff;
    rgb565[pixelIndex + 1] = (rgb565Pixel >> 8) & 0xff;
  }
  return rgb565;
}

describe('RGB565 conversion with jimp', () => {
  const targetWidth = 10;
  const targetHeight = 10;

  test('jimp produces RGBA bitmap (4 bytes per pixel)', async () => {
    // Create a test image with jimp
    const image = new Jimp({ width: 20, height: 20, color: 0xff0000ff }); // red
    image.resize({ w: targetWidth, h: targetHeight });

    // Jimp bitmap.data is always RGBA
    expect(image.bitmap.data.length).toBe(targetWidth * targetHeight * 4);
  });

  test('RGB565 output size is correct', async () => {
    const image = new Jimp({ width: 20, height: 20, color: 0x8040c0ff }); // purple
    image.resize({ w: targetWidth, h: targetHeight });

    const rgb565 = convertToRgb565(image.bitmap.data, targetWidth, targetHeight);
    expect(rgb565.length).toBe(targetWidth * targetHeight * 2);
  });

  test('can read and resize JPEG buffer', async () => {
    // Create a JPEG buffer
    const original = new Jimp({ width: 100, height: 100, color: 0x00ff00ff }); // green
    const jpegBuffer = await original.getBuffer('image/jpeg');

    // Read it back and resize
    const image = await Jimp.read(jpegBuffer);
    image.resize({ w: targetWidth, h: targetHeight });

    expect(image.bitmap.width).toBe(targetWidth);
    expect(image.bitmap.height).toBe(targetHeight);
  });
});
