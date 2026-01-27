import { test, expect } from '@playwright/test';

const BASE_URL = process.env.UHC_URL || 'http://192.168.1.2:8088';

test.describe('Mobile Responsive Layout', () => {
  test('zones page shows single column on iPhone', async ({ page }) => {
    // iPhone 15 Pro viewport
    await page.setViewportSize({ width: 393, height: 852 });
    await page.goto(`${BASE_URL}/`);

    // Wait for zones to load
    await page.waitForSelector('.zone-card', { timeout: 10000 });

    // Get the grid container
    const grid = page.locator('.grid').first();

    // Check computed grid-template-columns - should be single column on mobile
    const columns = await grid.evaluate((el) => {
      return window.getComputedStyle(el).gridTemplateColumns;
    });

    // Single column means one value (the width), not multiple
    const columnCount = columns.split(' ').filter(c => c && c !== '0px').length;
    expect(columnCount).toBe(1);
  });

  test('zone cards do not overflow viewport', async ({ page }) => {
    // iPhone 15 Pro viewport
    await page.setViewportSize({ width: 393, height: 852 });
    await page.goto(`${BASE_URL}/`);
    await page.waitForSelector('.zone-card', { timeout: 10000 });

    const viewportWidth = page.viewportSize()?.width || 393;

    // Check each zone card doesn't exceed viewport
    const cards = page.locator('.zone-card');
    const count = await cards.count();

    for (let i = 0; i < count; i++) {
      const box = await cards.nth(i).boundingBox();
      expect(box).not.toBeNull();
      if (box) {
        expect(box.width).toBeLessThanOrEqual(viewportWidth);
        expect(box.x).toBeGreaterThanOrEqual(0);
        expect(box.x + box.width).toBeLessThanOrEqual(viewportWidth + 1); // +1 for rounding
      }
    }
  });
});

test.describe('HQPlayer Zone Linking', () => {
  test('Link Zone button links the selected zone', async ({ page }) => {
    await page.goto(`${BASE_URL}/hqplayer`);

    // Wait for page to load (don't use networkidle - SSE keeps connection open)
    await page.waitForLoadState('domcontentloaded');

    // Check if already linked - if so, unlink first
    const unlinkButton = page.locator('button:has-text("Unlink")');
    if (await unlinkButton.isVisible()) {
      await unlinkButton.click();
      await page.waitForTimeout(500);
    }

    // Now we should see the zone dropdown and Link button
    const linkButton = page.locator('button:has-text("Link Zone")');
    await expect(linkButton).toBeVisible({ timeout: 5000 });

    // Get the zone dropdown
    const zoneDropdown = page.locator('select[aria-label="Select zone to link"]');
    await expect(zoneDropdown).toBeVisible();

    // Get the first zone option value
    const firstOption = zoneDropdown.locator('option').first();
    const zoneId = await firstOption.getAttribute('value');
    const zoneName = await firstOption.textContent();

    expect(zoneId).toBeTruthy();
    expect(zoneName).toBeTruthy();

    // Click Link Zone
    await linkButton.click();

    // Wait for the link to be established
    await page.waitForTimeout(1000);

    // Should now show "Linked to [zone name]" and an Unlink button
    await expect(page.locator(`text=Linked to`)).toBeVisible({ timeout: 5000 });
    await expect(unlinkButton).toBeVisible();

    // Clean up - unlink
    await unlinkButton.click();
  });
});
