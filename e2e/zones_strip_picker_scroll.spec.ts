import { test, expect, type Page } from '@playwright/test';
import { readFileSync } from 'node:fs';

// The web build emits this file from src/input.css. Keeping the path
// configurable lets the test exercise the exact compiled CSS in CI and in a
// local standalone fixture without starting the full Bridge server.
const compiledCssPath = process.env.UHC_PICKER_CSS || 'public/tailwind.css';
const compiledCss = readFileSync(compiledCssPath, 'utf8');

function pickerMarkup() {
  const rows = Array.from({ length: 60 }, (_, index) =>
    `<li><div class="zones-strip-picker-row"><button class="zones-strip-picker-item" type="button"><div class="zones-strip-picker-meta"><span class="zones-strip-zone-name">Player ${index + 1}</span><span class="zones-strip-track zones-strip-track--muted">Track ${index + 1}</span></div></button></div></li>`,
  ).join('');

  return `
    <main style="height: 1800px; padding: 1rem"><h1>Library</h1></main>
    <div class="zones-strip">
      <div class="zones-strip-inner">
        <button class="zones-strip-target" type="button"><span>Current player</span></button>
        <div class="zones-strip-rail"><button class="btn" type="button">Play</button></div>
      </div>
      <div class="zones-strip-picker">
        <ul class="zones-strip-picker-list">${rows}</ul>
      </div>
    </div>`;
}

async function loadFixture(page: Page) {
  await page.setContent(`<!doctype html><style>${compiledCss}</style>${pickerMarkup()}`);
}

test.describe('long library player picker', () => {
  test('keeps wheel scrolling inside the list and contains edge scroll chaining', async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.emulateMedia({ reducedMotion: 'reduce' });
    await loadFixture(page);

    await page.evaluate(() => window.scrollTo(0, 300));
    const list = page.locator('.zones-strip-picker-list');
    const listBox = await list.boundingBox();
    expect(listBox).not.toBeNull();
    if (!listBox) return;

    const pageBefore = await page.evaluate(() => window.scrollY);
    await page.mouse.move(listBox.x + listBox.width / 2, listBox.y + listBox.height / 2);
    await page.mouse.wheel(0, 500);
    await page.waitForTimeout(100);
    await expect.poll(() => list.evaluate((element) => element.scrollTop)).toBeGreaterThan(0);
    expect(await page.evaluate(() => window.scrollY)).toBe(pageBefore);

    await list.evaluate((element) => { element.scrollTop = element.scrollHeight; });
    await page.waitForTimeout(300);
    await page.mouse.move(10, 10);
    await page.mouse.move(listBox.x + listBox.width / 2, listBox.y + listBox.height / 2);
    const pageAtBottom = await page.evaluate(() => window.scrollY);
    await page.mouse.wheel(0, 1000);
    await page.waitForTimeout(250);
    expect(await page.evaluate(() => window.scrollY)).toBe(pageAtBottom);
  });

  test('keeps the picker reachable on a short mobile viewport', async ({ page }) => {
    await page.setViewportSize({ width: 393, height: 400 });
    await page.emulateMedia({ reducedMotion: 'reduce' });
    await loadFixture(page);

    const viewportHeight = page.viewportSize()?.height ?? 400;
    const picker = page.locator('.zones-strip-picker');
    const box = await picker.boundingBox();
    expect(box).not.toBeNull();
    if (box) {
      expect(box.y).toBeGreaterThanOrEqual(0);
      expect(box.y + box.height).toBeLessThanOrEqual(viewportHeight);
    }

    const list = page.locator('.zones-strip-picker-list');
    expect(await list.evaluate((element) => element.scrollHeight > element.clientHeight)).toBe(true);
    await list.evaluate((element) => { element.scrollTop = element.scrollHeight; });
    const lastRow = list.locator('li').last();
    const lastRowBox = await lastRow.boundingBox();
    expect(lastRowBox).not.toBeNull();
    if (lastRowBox) expect(lastRowBox.y + lastRowBox.height).toBeLessThanOrEqual(viewportHeight);
  });
});
