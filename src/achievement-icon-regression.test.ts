// @vitest-environment happy-dom
import { afterEach, expect, it, vi } from 'vitest';
import type { AchievementCatalog } from './types';

const api=vi.hoisted(()=>Object.fromEntries([
  'getEnvironment','loadSettings','scanSteam','scanLuaLibrary',
  'detectKernelInstallation','searchCatalog','loadGameCover',
  'listAchievements','loadAchievementImage','applyAchievementChanges',
].map(name=>[name,vi.fn()])));
vi.mock('./api',()=>api);
afterEach(()=>vi.unstubAllGlobals());

it('does not block localized rows on images, shares inflight images on rerender, and isolates switched games',async()=>{
  vi.resetModules();Object.values(api).forEach(mock=>mock.mockReset());localStorage.clear();
  vi.stubGlobal('IntersectionObserver',undefined);
  document.body.innerHTML='<div id="app"></div>';history.replaceState(null,'','#kernel');
  Object.defineProperty(window,'matchMedia',{configurable:true,value:()=>({matches:true,addEventListener:vi.fn()})});
  Object.defineProperty(window,'confirm',{configurable:true,value:vi.fn(()=>true)});
  HTMLElement.prototype.scrollTo=vi.fn();HTMLElement.prototype.scrollIntoView=vi.fn();
  api.getEnvironment.mockResolvedValue({locale:'zh-CN',version:'0.2.2',desktop:true});
  api.loadSettings.mockResolvedValue({language:'zh-CN',kernelPath:null,steamPath:'C:\\Fixture\\Steam'});
  api.scanSteam.mockResolvedValue({capturedAt:1,steam:{path:'C:\\Fixture\\Steam'},warnings:[],apps:[42,43].map(appid=>({appid,name:`Game ${appid}`,installDir:'x',manifestPath:'x',libraryPath:'x',sizeBytes:0,stateFlags:4,lastUpdated:0}))});
  api.scanLuaLibrary.mockResolvedValue({layout:{},warnings:[],entries:[42,43].map(appId=>({appId,fileName:`${appId}.lua`,enabled:true,appIds:[appId],pinnedManifestCount:0}))});
  api.detectKernelInstallation.mockResolvedValue(null);
  api.searchCatalog.mockResolvedValue([]);api.loadGameCover.mockResolvedValue(null);
  api.listAchievements.mockImplementation(async(_path,appId):Promise<AchievementCatalog>=>({appId,requestedLanguage:'schinese',resolvedLanguage:'schinese',source:'steam-client',connected:true,writable:true,error:null,achievements:[{id:'FIRST',displayName:`已本地化的成就 ${appId}`,description:'名称和操作不等待图标下载。',icon:`color-${appId}.jpg`,iconGray:`gray-${appId}.jpg`,unlocked:false,unlockTime:null,hidden:false,protected:false}]}));
  const resolvers=new Map<number,(value:unknown)=>void>();
  api.loadAchievementImage.mockImplementation((appId,_image,_fallback,cacheOnly)=>cacheOnly?Promise.resolve(null):new Promise(resolve=>resolvers.set(appId,resolve)));
  await import('./main');
  await vi.waitFor(()=>expect(document.querySelector('.progress-strip')).toBeNull());
  document.querySelector<HTMLButtonElement>('[data-page="achievements"]')!.click();
  document.querySelector<HTMLButtonElement>('[data-action="achievement-select:42"]')!.click();
  await vi.waitFor(()=>expect(document.querySelector('.achievement-row')?.textContent).toContain('已本地化的成就 42'));
  expect(document.querySelector<HTMLButtonElement>('[data-action="achievement-unlock:FIRST"]')?.disabled).toBe(false);
  expect(document.querySelector('.achievement-loading')).toBeNull();
  await vi.waitFor(()=>expect(resolvers.has(42)).toBe(true));
  const oldImage=document.querySelector<HTMLImageElement>('[data-achievement-image]')!;
  const checked=document.querySelector<HTMLInputElement>('[data-achievement-id="FIRST"]')!;
  checked.checked=true;checked.dispatchEvent(new Event('change',{bubbles:true}));
  await new Promise(resolve=>setTimeout(resolve,20));
  expect(oldImage.isConnected).toBe(false);
  expect(api.loadAchievementImage.mock.calls.filter(call=>call[0]===42&&!call[3])).toHaveLength(1);
  document.querySelector<HTMLButtonElement>('[data-achievement-tab="library"]')!.click();
  document.querySelector<HTMLButtonElement>('[data-action="achievement-select:43"]')!.click();
  await vi.waitFor(()=>expect(document.querySelector('.achievement-row')?.textContent).toContain('已本地化的成就 43'));
  await vi.waitFor(()=>expect(resolvers.has(43)).toBe(true));
  const currentImage=document.querySelector<HTMLImageElement>('[data-achievement-image]')!;
  const otherData='data:image/png;base64,Z2FtZTQy';
  resolvers.get(42)!({appId:42,dataUrl:otherData,source:'network'});
  await new Promise(resolve=>setTimeout(resolve,10));
  expect(currentImage.getAttribute('src')).not.toBe(otherData);
  const currentData='data:image/png;base64,Z2FtZTQz';
  resolvers.get(43)!({appId:43,dataUrl:currentData,source:'network'});
  await vi.waitFor(()=>expect(currentImage.getAttribute('src')).toBe(currentData));
  expect(api.applyAchievementChanges).not.toHaveBeenCalled();
});
