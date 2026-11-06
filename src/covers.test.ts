// @vitest-environment happy-dom
import { expect, it, vi } from 'vitest';
import { CoverLoader, steamCoverCandidates } from './covers';
import type { CoverRequest } from './covers';
const request=(appId:number):CoverRequest=>({appId,steamPath:'fixture',language:'en',candidates:[],desktop:true});
it('shares duplicate requests, bounds concurrency and keeps successes across renders',async()=>{
  const resolvers=new Map<number,(value:unknown)=>void>();let active=0,max=0;
  const transport=vi.fn((r:CoverRequest)=>new Promise<never>(resolve=>{active++;max=Math.max(max,active);resolvers.set(r.appId,value=>{active--;resolve(value as never)})}));
  const loader=new CoverLoader(transport,6);
  const first=loader.load(request(1));expect(loader.load(request(1))).toBe(first);
  const pending=[first,...Array.from({length:8},(_,i)=>loader.load(request(i+2)))];
  await Promise.resolve();expect(transport).toHaveBeenCalledTimes(6);
  for(let id=1;id<=9;id++){
    await vi.waitFor(()=>expect(resolvers.has(id)).toBe(true));
    resolvers.get(id)!({appId:id,dataUrl:`data:image/png;base64,${id}`,source:'localSteam'});
  }
  await Promise.all(pending);expect(max).toBe(6);
  const calls=transport.mock.calls.length;expect((await loader.load(request(1)))?.source).toBe('localSteam');
  expect(transport).toHaveBeenCalledTimes(calls);
});
it('does not retry every failure on every DOM render',async()=>{
  const transport=vi.fn(async()=>null),loader=new CoverLoader(transport);
  await loader.load(request(1));await loader.load(request(1));expect(transport).toHaveBeenCalledTimes(1);
});
it('starts with valid horizontal endpoints and preserves hashed Store URLs',()=>{
  const hash='https://shared.akamai.steamstatic.com/store_item_assets/steam/apps/42/hash/header.jpg';
  const urls=steamCoverCandidates(42,[hash,hash]);expect(urls[0]).toBe(hash);expect(urls).not.toContain('https://shared.cloudflare.steamstatic.com/store_item_assets/steam/apps/42/header.jpg');
  expect(new Set(urls).size).toBe(urls.length);
  expect(steamCoverCandidates(42,['file:///C:/secret','https://steamstatic.com.evil.test/a','not-a-url'])).toEqual(steamCoverCandidates(42));
});
