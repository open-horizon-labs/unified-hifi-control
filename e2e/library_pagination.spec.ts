import { expect, test } from '@playwright/test';

const baseUrl = process.env.UHC_URL || 'http://192.168.1.2:8088';
const initialLocation = 'loc_WFoj90WCkO8tsIQmgM8rKA';

function collectionPage(offset: number, location: string) {
  const items = Array.from({ length: offset < 60 ? 30 : 7 }, (_, index) => {
    const number = offset + index + 1;
    return {
      title: `Folder ${number}`,
      location: `loc_folder_${number}`,
    };
  });
  return {
    outcome: 'ok',
    data: {
      items,
      breadcrumbs: [{ title: 'Library', location }],
      next_offset: offset < 60 ? offset + 30 : null,
    },
  };
}

async function installLibraryMocks(page: import('@playwright/test').Page) {
  const collectionRequests: Array<{ offset: number; location: string | null }> = [];

  await page.route('**/zones', (route) => route.fulfill({
    contentType: 'application/json',
    body: JSON.stringify({
      zones: [{
        zone_id: 'roon:mock-zone',
        zone_name: 'Mock Roon Zone',
        source: 'roon',
        browse_supported: true,
        library_tabs: ['browse'],
      }],
    }),
  }));
  await page.route('**/now_playing**', (route) => route.fulfill({
    contentType: 'application/json',
    body: JSON.stringify({ line1: 'Idle', is_playing: false }),
  }));
  await page.route('**/api/collections', async (route) => {
    const body = route.request().postDataJSON() as { offset?: number; location?: string };
    const offset = body.offset ?? 0;
    const location = body.location ?? null;
    collectionRequests.push({ offset, location });
    await route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify(collectionPage(offset, location ?? initialLocation)),
    });
  });

  return collectionRequests;
}

test('Library pagination settles, advances offsets, retains rows, and refetches after navigation', async ({ page }) => {
  const collectionRequests = await installLibraryMocks(page);
  await page.goto(`${baseUrl}/library/roon/browse/${initialLocation}`, { waitUntil: 'domcontentloaded' });

  const more = page.getByRole('button', { name: 'Load more', exact: true });
  await expect(page.getByText('Folder 1', { exact: true })).toBeVisible({ timeout: 10_000 });
  await expect.poll(() => collectionRequests.map(({ offset }) => offset)).toEqual([0]);

  await more.click();
  await expect(page.getByText('Folder 30', { exact: true })).toBeVisible();
  await expect.poll(() => collectionRequests.map(({ offset }) => offset)).toEqual([0, 30]);

  await more.click();
  await expect(page.getByText('Folder 60', { exact: true })).toBeVisible();
  await expect(page.getByText('Folder 67', { exact: true })).toBeVisible();
  await expect.poll(() => collectionRequests.map(({ offset }) => offset)).toEqual([0, 30, 60]);

  await page.getByRole('button', { name: 'Open Folder 1', exact: true }).click();
  await expect(page).toHaveURL(/\/library\/roon\/browse\/loc_folder_1$/);
  await expect.poll(() => collectionRequests.at(-1)).toEqual({ offset: 0, location: 'loc_folder_1' });
  await expect(page.getByText('Folder 1', { exact: true })).toBeVisible();
});
