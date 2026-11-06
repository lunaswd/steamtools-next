// @vitest-environment happy-dom
import { afterEach, expect, it, vi } from 'vitest';
const api=vi.hoisted(()=>({loadAchievementImage:vi.fn(),loadGameCover:vi.fn()}));
vi.mock('./api',()=>api);
import { achievementImageCandidates, achievementImageMarkup, bindAchievementImages } from './achievement-images';
afterEach(()=>vi.unstubAllGlobals());
it('rewrites legacy Steam community icons to modern assets and rejects unrelated paths',()=>{
  const candidates=achievementImageCandidates(42,'https://cdn.steamstatic.com/steamcommunity/public/images/apps/42/icon.jpg');
  expect(candidates[0]).toBe('https://shared.akamai.steamstatic.com/community_assets/images/apps/42/icon.jpg');
  expect(achievementImageCandidates(42,'https://evil.test/icon.jpg')).toEqual([]);
  expect(achievementImageCandidates(42,'../icon.jpg')).toEqual([]);
  expect(achievementImageCandidates(42,'https://shared.akamai.steamstatic.com/community_assets/images/apps/99/icon.jpg')).toEqual([]);
});
it('waits for visible icons, uses the independent cache lane, and reuses image bytes after rerender',async()=>{
  let callback!:IntersectionObserverCallback;let observed!:Element;
  vi.stubGlobal('IntersectionObserver',class{constructor(cb:IntersectionObserverCallback){callback=cb}observe(node:Element){observed=node}unobserve(){}disconnect(){}});
  const data='data:image/jpeg;base64,aWNvbg==';
  api.loadAchievementImage.mockImplementation(async(_id,_image,_fallback,cacheOnly)=>cacheOnly?null:{appId:42,dataUrl:data,source:'network'});
  document.body.innerHTML=`<div id="icons">${achievementImageMarkup(42,'visible-test.jpg','color-test.jpg',true)}</div>`;
  const root=document.querySelector<HTMLElement>('#icons')!;bindAchievementImages(root,true);
  expect(api.loadAchievementImage).not.toHaveBeenCalled();
  callback([{target:observed,isIntersecting:true} as IntersectionObserverEntry],{} as IntersectionObserver);
  await vi.waitFor(()=>expect(root.querySelector('img')?.getAttribute('src')).toBe(data));
  expect(api.loadAchievementImage).toHaveBeenCalledWith(42,'visible-test.jpg',null,true);
  expect(api.loadAchievementImage).toHaveBeenCalledWith(42,'visible-test.jpg','color-test.jpg');
  const calls=api.loadAchievementImage.mock.calls.length;
  root.innerHTML=achievementImageMarkup(42,'visible-test.jpg','color-test.jpg',true);bindAchievementImages(root,true);
  expect(root.querySelector('img')?.getAttribute('src')).toBe(data);
  callback([{target:observed,isIntersecting:true} as IntersectionObserverEntry],{} as IntersectionObserver);
  await Promise.resolve();expect(api.loadAchievementImage).toHaveBeenCalledTimes(calls);
});
