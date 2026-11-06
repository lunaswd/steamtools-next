// @vitest-environment happy-dom
import { beforeEach, expect, it, vi } from 'vitest';

const api = vi.hoisted(() => Object.fromEntries([
  'getEnvironment', 'loadSettings', 'scanSteam', 'scanLuaLibrary',
  'detectKernelInstallation', 'searchCatalog', 'loadGameCover',
].map(name => [name, vi.fn()])));
vi.mock('./api', () => api);

beforeEach(async () => {
  vi.resetModules(); Object.values(api).forEach(fn => fn.mockReset()); localStorage.clear();
  document.body.innerHTML = '<div id="app"></div>';
  history.replaceState(null, '', '#kernel');
  Object.defineProperty(window, 'matchMedia', { configurable: true, value: () => ({ matches: true, addEventListener: vi.fn() }) });
  HTMLElement.prototype.scrollTo = vi.fn();
  api.getEnvironment.mockResolvedValue({ locale: 'en', version: '0.2.1', desktop: true });
  api.loadSettings.mockResolvedValue({ language: 'en', kernelPath: null, steamPath: 'C:\\Fixture\\Steam' });
  api.scanSteam.mockResolvedValue({ capturedAt: 1, steam: { path: 'C:\\Fixture\\Steam' }, apps: [], warnings: [] });
  api.scanLuaLibrary.mockResolvedValue({ layout: {}, entries: [42, 43, 44].map(appId => ({ appId, fileName: `${appId}.lua`, enabled: true, appIds: [appId], pinnedManifestCount: 0 })), warnings: [] });
  api.detectKernelInstallation.mockResolvedValue(null);
  api.loadGameCover.mockResolvedValue(null);
});

it('starts metadata work concurrently instead of waiting for each game', async () => {
  api.searchCatalog.mockImplementation(() => new Promise(() => {}));
  await import('./main');
  await vi.waitFor(() => expect(document.querySelector('.progress-strip')).toBeNull());
  document.querySelector<HTMLButtonElement>('[data-page="library"]')!.click();
  await vi.waitFor(() => expect(api.searchCatalog).toHaveBeenCalledTimes(3), { timeout: 300, interval: 5 });
});

it('does not rebuild already loading cover elements when metadata arrives', async () => {
  const resolvers=new Map<string,(value:unknown)=>void>();
  api.searchCatalog.mockImplementation((id:string) => new Promise(done => { resolvers.set(id,done); }));
  await import('./main');
  await vi.waitFor(() => expect(document.querySelector('.progress-strip')).toBeNull());
  document.querySelector<HTMLButtonElement>('[data-page="library"]')!.click();
  // Let the navigation/hashchange lifecycle settle before observing metadata.
  await new Promise(resolve=>setTimeout(resolve,20));
  const image = document.querySelector('.lua-card-cover img');
  resolvers.get('42')!([{ appId: 42, name: 'Localized game', coverUrl: '', coverCandidates: [] }]);
  await vi.waitFor(() => expect(document.querySelector('.page-content')?.textContent).toContain('Localized game'));
  expect(document.querySelector('.lua-card-cover img')).toBe(image);
});
