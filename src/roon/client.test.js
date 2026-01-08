/**
 * SAFETY CRITICAL: Volume control regression tests
 *
 * Bug: vol_abs used hardcoded 0-100 range, causing dB values like -12
 * to be clamped to 0 (maximum volume), risking equipment damage.
 *
 * Fix: Use zone's actual volume range (e.g., -64 to 0 dB).
 */

const { _test } = require('./client');
const { clamp, getVolumeRange } = _test;

describe('Volume control safety - zone range handling', () => {
  describe('dB scale zones (HQPlayer)', () => {
    const dbOutput = {
      output_id: 'hqp',
      volume: { type: 'db', min: -64, max: 0, value: -20 },
    };

    test('respects dB range', () => {
      const range = getVolumeRange(dbOutput);
      expect(range).toEqual({ min: -64, max: 0 });
    });

    test('CRITICAL: -12 dB stays -12 dB (not clamped to 0)', () => {
      const { min, max } = getVolumeRange(dbOutput);
      expect(clamp(-12, min, max)).toBe(-12);
    });

    test('values below zone min are clamped to zone min', () => {
      const { min, max } = getVolumeRange(dbOutput);
      expect(clamp(-100, min, max)).toBe(-64);
    });

    test('values above zone max are clamped to zone max', () => {
      const { min, max } = getVolumeRange(dbOutput);
      expect(clamp(10, min, max)).toBe(0);
    });
  });

  describe('percentage scale zones (typical devices)', () => {
    const pctOutput = {
      output_id: 'sonos',
      volume: { type: 'number', min: 0, max: 100, value: 50 },
    };

    test('respects 0-100 range', () => {
      const range = getVolumeRange(pctOutput);
      expect(range).toEqual({ min: 0, max: 100 });
    });

    test('50 stays 50', () => {
      const { min, max } = getVolumeRange(pctOutput);
      expect(clamp(50, min, max)).toBe(50);
    });

    test('values below 0 are clamped to 0', () => {
      const { min, max } = getVolumeRange(pctOutput);
      expect(clamp(-10, min, max)).toBe(0);
    });

    test('values above 100 are clamped to 100', () => {
      const { min, max } = getVolumeRange(pctOutput);
      expect(clamp(150, min, max)).toBe(100);
    });
  });

  describe('missing volume info (fallback)', () => {
    test('uses safe defaults when volume missing', () => {
      const range = getVolumeRange({});
      expect(range).toEqual({ min: 0, max: 100 });
    });

    test('uses safe defaults when output undefined', () => {
      const range = getVolumeRange(undefined);
      expect(range).toEqual({ min: 0, max: 100 });
    });
  });

  describe('clamp edge cases', () => {
    test('NaN returns min', () => {
      expect(clamp(NaN, -64, 0)).toBe(-64);
      expect(clamp(NaN, 0, 100)).toBe(0);
    });

    test('value exactly at boundaries', () => {
      expect(clamp(-64, -64, 0)).toBe(-64);
      expect(clamp(0, -64, 0)).toBe(0);
      expect(clamp(0, 0, 100)).toBe(0);
      expect(clamp(100, 0, 100)).toBe(100);
    });
  });
});
