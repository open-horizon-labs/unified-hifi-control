import { expect, test } from '@playwright/test';

for (const width of [1280, 390, 320]) {
  test(`Library footer stays above the playback bar at ${width}px`, async ({ page }) => {
    await page.setViewportSize({ width, height: 800 });
    await page.route('**/zones', route => route.fulfill({ json: { zones: [{
      zone_id: 'roon:footer-test', zone_name: 'Footer test room', source: 'roon',
      browse_supported: true, library_tabs: ['browse'],
    }] } }));
    await page.route('**/now_playing**', route => route.fulfill({
      json: { line1: 'Test recording', line2: 'Test artist', is_playing: false },
    }));
    await page.route('**/api/collections', route => route.fulfill({ json: {
      outcome: 'ok', data: { items: [], breadcrumbs: [], next_offset: null },
    } }));
    await page.goto('/');
    const strip = page.locator('.zones-strip');
    await expect(strip).toBeVisible();
    const footer = page.locator('footer');
    const support = footer.getByRole('link', { name: 'Buy me a coffee' });
    await support.scrollIntoViewIfNeeded();
    await page.evaluate(() => window.scrollTo(0, document.documentElement.scrollHeight));
    await expect(support).toBeInViewport();
    await expect.poll(() => support.evaluate(link => {
      const box = link.getBoundingClientRect();
      const hit = document.elementFromPoint(box.x + box.width / 2, box.y + box.height / 2);
      return hit === link || (hit !== null && link.contains(hit));
    })).toBe(true);
    await expect(footer).not.toContainText('Local control, optional Cloud');

    // The same mounted bar must update clearance when it wraps after a resize.
    await page.setViewportSize({ width: width === 1280 ? 320 : 1280, height: 800 });
    await support.scrollIntoViewIfNeeded();
    await page.evaluate(() => window.scrollTo(0, document.documentElement.scrollHeight));
    await expect.poll(async () => {
      const link = await support.boundingBox();
      const bar = await strip.boundingBox();
      return link !== null && bar !== null && link.y + link.height <= bar.y;
    }).toBe(true);
  });
}
