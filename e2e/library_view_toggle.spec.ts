import { expect, test } from '@playwright/test';
import { join } from 'node:path';
import { readdirSync } from 'node:fs';

const baseUrl = process.env.UHC_URL || 'http://192.168.1.2:8088';
const location = 'loc_test_root';

async function installLibraryMocks(page: import('@playwright/test').Page) {
  const assetDir = process.env.UHC_CLIENT_ASSET_DIR;
  if (assetDir) {
    const mainJs = readdirSync(assetDir).find((name) => /^unified-hifi-control-dx.*\.js$/.test(name));
    const wasm = readdirSync(assetDir).find((name) => /^unified-hifi-control_bg-.*\.wasm$/.test(name));
    if (!mainJs || !wasm) throw new Error(`UHC_CLIENT_ASSET_DIR lacks the main JS/WASM pair: ${assetDir}`);
    await page.route('**/assets/unified-hifi-control*.js', (route) =>
      route.fulfill({ path: join(assetDir, mainJs), contentType: 'text/javascript' }));
    await page.route('**/assets/unified-hifi-control*.wasm', (route) =>
      route.fulfill({ path: join(assetDir, wasm), contentType: 'application/wasm' }));
  }

  await page.route('**/zones', (route) => route.fulfill({
    contentType: 'application/json',
    body: JSON.stringify({ zones: [{
      zone_id: 'roon:mock-zone', zone_name: 'Mock Roon Zone', source: 'roon',
      browse_supported: true, library_tabs: ['browse'],
    }] }),
  }));
  await page.route('**/now_playing**', (route) => route.fulfill({
    contentType: 'application/json', body: JSON.stringify({ line1: 'Idle', is_playing: false }),
  }));
  const collectionRequests: Array<{ offset: number; location: string | null }> = [];
  await page.route('**/api/collections', async (route) => {
    const body = route.request().postDataJSON() as { offset?: number; location?: string };
    const offset = body.offset ?? 0;
    const requestedLocation = body.location ?? null;
    collectionRequests.push({ offset, location: requestedLocation });
    const child = requestedLocation === 'loc_folder_1';
    const start = child ? 1 : offset === 30 ? 31 : 1;
    const end = child ? 3 : offset === 30 ? 35 : 30;
    return route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify({ outcome: 'ok', data: {
        items: Array.from({ length: end - start + 1 }, (_, index) => ({
          title: child ? `Child ${start + index}` : `Folder ${start + index}`,
          location: child ? `loc_child_${start + index}` : `loc_folder_${start + index}`,
        })),
        breadcrumbs: [{ title: 'Library', location: requestedLocation ?? location }],
        next_offset: child || offset === 30 ? null : 30,
      } }),
    });
  });
  return collectionRequests;
}

test('Library lets the user choose and persist List or Cards without refetching', async ({ page }) => {
  const collectionRequests = await installLibraryMocks(page);

  await page.goto(`${baseUrl}/library/roon/browse/${location}`, { waitUntil: 'domcontentloaded' });
  await expect(page.getByText('Folder 1', { exact: true })).toBeVisible({ timeout: 10_000 });
  const listView = page.getByRole('button', { name: 'List view', exact: true });
  const cardsView = page.getByRole('button', { name: 'Cards view', exact: true });
  await expect(listView).toHaveAttribute('aria-pressed', 'true');
  await expect(cardsView).toHaveAttribute('aria-pressed', 'false');

  const loadMore = page.getByRole('button', { name: 'Load more', exact: true });
  await expect(loadMore).toBeVisible();
  await loadMore.click();
  await expect(page.getByText('Folder 31', { exact: true })).toBeVisible();
  await expect(page.getByText('Folder 35', { exact: true })).toBeVisible();
  await expect(page.locator('.library-row')).toHaveCount(35);
  await expect(loadMore).toBeHidden();
  await expect.poll(() => collectionRequests.map(({ offset }) => offset)).toEqual([0, 30]);

  const requestsBeforeToggle = collectionRequests.length;
  await cardsView.click();
  await expect(page.locator('.library-grid')).toBeVisible();
  await expect(page.getByText('Folder 31', { exact: true })).toBeVisible();
  await expect(page.getByText('Folder 35', { exact: true })).toBeVisible();
  await expect(cardsView).toHaveAttribute('aria-pressed', 'true');
  await expect(listView).toHaveAttribute('aria-pressed', 'false');
  await page.waitForTimeout(250);
  expect(collectionRequests.length).toBe(requestsBeforeToggle);
  await expect.poll(() => page.evaluate(() => localStorage.getItem('uhc.library.view'))).toBe('cards');

  await listView.click();
  await expect(page.locator('.library-list')).toBeVisible();
  await expect(page.getByText('Folder 35', { exact: true })).toBeVisible();
  await page.waitForTimeout(250);
  expect(collectionRequests.length).toBe(requestsBeforeToggle);

  await cardsView.click();
  await expect(page.getByText('Folder 35', { exact: true })).toBeVisible();
  await page.waitForTimeout(250);
  expect(collectionRequests.length).toBe(requestsBeforeToggle);
  await page.locator('.library-tile').filter({
    has: page.getByText('Folder 1', { exact: true }),
  }).click();
  await expect(page).toHaveURL(/\/library\/roon\/browse\/loc_folder_1$/);
  await expect(page.getByText('Child 1', { exact: true })).toBeVisible();
  await expect(page.locator('.library-grid')).toBeVisible();
  await expect(page.getByRole('button', { name: 'Cards view', exact: true })).toHaveAttribute('aria-pressed', 'true');
  await expect.poll(() => collectionRequests.at(-1)).toEqual({ offset: 0, location: 'loc_folder_1' });

  await page.reload({ waitUntil: 'domcontentloaded' });
  await expect(page.locator('.library-grid')).toBeVisible();
  await expect(page.getByRole('button', { name: 'Cards view', exact: true })).toHaveAttribute('aria-pressed', 'true');

  await page.evaluate(() => localStorage.setItem('uhc.library.view', 'invalid'));
  await page.reload({ waitUntil: 'domcontentloaded' });
  await expect(page.getByRole('button', { name: 'List view', exact: true })).toHaveAttribute('aria-pressed', 'true');
});
