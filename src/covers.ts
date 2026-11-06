import { loadGameCover } from './api';
import type { GameCover } from './types';
import './covers.css';

export interface CoverRequest { steamPath:string|null; appId:number; language:string; candidates:string[]; desktop:boolean; resourceId?:string }
type Task = {request:CoverRequest; resolve:(value:GameCover|null)=>void};
const key = (r:CoverRequest) => `${r.steamPath||'preview'}:${r.language}:${r.appId}:${r.resourceId||'cover'}`;

/** A shared queue, independent of DOM lifetime. Recent + installed copies of one
 * game share a request; rerenders reuse bytes instead of restarting downloads. */
export class CoverLoader {
  private active=0;
  private queue:Task[]=[];
  private pending=new Map<string,Promise<GameCover|null>>();
  private cache=new Map<string,GameCover>();
  private failed=new Map<string,number>();
  private cacheBytes=0;
  constructor(private transport:(request:CoverRequest)=>Promise<GameCover|null>, private concurrency=6,private maxCacheBytes=24*1024*1024) {}
  peek(request:CoverRequest){return this.cache.get(key(request))||null}
  remember(request:CoverRequest,cover:GameCover){this.put(key(request),cover);this.failed.delete(key(request))}
  private put(id:string,cover:GameCover){
    this.cacheBytes-=2*(this.cache.get(id)?.dataUrl.length||0);this.cache.set(id,cover);this.cacheBytes+=2*cover.dataUrl.length;
    // Cover queues retain 24 MiB each; achievement queues 8 MiB each, for
    // 64 MiB total, conservatively counting encoded strings as UTF-16.
    while(this.cache.size>300||this.cacheBytes>this.maxCacheBytes){const oldest=this.cache.keys().next().value!;this.cacheBytes-=2*this.cache.get(oldest)!.dataUrl.length;this.cache.delete(oldest)}
  }
  load(request:CoverRequest):Promise<GameCover|null>{
    const id=key(request), cached=this.cache.get(id);
    if(cached)return Promise.resolve(cached);
    if((this.failed.get(id)||0)>Date.now())return Promise.resolve(null);
    const pending=this.pending.get(id);if(pending)return pending;
    const task=new Promise<GameCover|null>(resolve=>this.queue.push({request,resolve}));
    this.pending.set(id,task);this.pump();return task;
  }
  private pump(){while(this.active<this.concurrency&&this.queue.length){const task=this.queue.shift()!;this.active++;void Promise.resolve().then(()=>this.transport(task.request)).catch(()=>null).then(cover=>{
    const id=key(task.request);this.pending.delete(id);
    if(cover)this.put(id,cover);
    else {this.failed.set(id,Date.now()+15000);if(this.failed.size>300)this.failed.delete(this.failed.keys().next().value!)}
    task.resolve(cover);
  }).finally(()=>{this.active--;this.pump()})}}
}

export function steamCoverCandidates(appId:number, preferred:string[]=[]){
  return [...new Set([...preferred,
    `https://shared.akamai.steamstatic.com/store_item_assets/steam/apps/${appId}/header.jpg`,
    `https://cdn.akamai.steamstatic.com/steam/apps/${appId}/header.jpg`,
    `https://cdn.cloudflare.steamstatic.com/steam/apps/${appId}/header.jpg`,
    `https://cdn.steamstatic.com/steam/apps/${appId}/header.jpg`,
  ].filter((url):url is string=>typeof url==='string'&&isImageSource(url)))];
}

function isImageSource(raw:string){try{const url=new URL(raw);return url.protocol==='https:'&&!url.username&&!url.password&&(!url.port||url.port==='443')&&(url.hostname==='steamstatic.com'||url.hostname.endsWith('.steamstatic.com')||url.hostname==='steamcdn-a.akamaihd.net'||url.hostname==='heyboximg.com'||url.hostname.endsWith('.heyboximg.com')||['cdn.max-c.com','img.max-c.com','img1.max-c.com','img2.max-c.com','img3.max-c.com'].includes(url.hostname))}catch{return false}}

export function browserImage(url:string,timeout=2500):Promise<boolean>{
  return new Promise(resolve=>{
    const image=new Image();let done=false;
    const finish=(ok:boolean)=>{if(done)return;done=true;window.clearTimeout(timer);image.onload=null;image.onerror=null;resolve(ok)};
    const timer=window.setTimeout(()=>{finish(false);image.src=''},timeout);
    image.onload=()=>finish(image.naturalWidth>0);image.onerror=()=>finish(false);image.src=url;
  });
}
async function browserCover(request:CoverRequest):Promise<GameCover|null>{
  for(const url of request.candidates.slice(0,5))if(await browserImage(url))return{appId:request.appId,dataUrl:url,source:'browserCache'};
  return null;
}
export const coverLoader=new CoverLoader(request=>request.desktop
  ?loadGameCover(request.steamPath,request.appId,request.language,request.candidates)
  :browserCover(request));
export const cachedCoverLoader=new CoverLoader(request=>request.desktop
  ?loadGameCover(request.steamPath,request.appId,request.language,[],true)
  :Promise.resolve(null),12);

let observer:IntersectionObserver|null=null;
export function bindCoverImages(root:HTMLElement, context:{steamPath:string|null; language:string; desktop:boolean}){
  observer?.disconnect();observer=null;
  const start=(image:HTMLImageElement)=>{
    let preferred:string[]=[];try{const parsed=JSON.parse(image.dataset.coverCandidates||'[]');if(Array.isArray(parsed))preferred=parsed}catch{}
    const request:CoverRequest={...context,appId:Number(image.dataset.coverAppid),candidates:steamCoverCandidates(Number(image.dataset.coverAppid),preferred)};
    const set=(cover:GameCover|null)=>{
      if(!image.isConnected)return;
      if(cover){image.src=cover.dataUrl;image.dataset.coverSource=cover.source;image.parentElement?.classList.remove('missing')}
      else image.parentElement?.classList.add('missing');
    };
    const cached=coverLoader.peek(request);if(cached){set(cached);return}
    image.dataset.coverLoading='true';void cachedCoverLoader.load(request).then(local=>{
      if(local){coverLoader.remember(request,local);set(local)}else void coverLoader.load(request).then(set);
    });
  };
  if(typeof IntersectionObserver!=='undefined')observer=new IntersectionObserver(entries=>{
    for(const entry of entries)if(entry.isIntersecting){observer?.unobserve(entry.target);start(entry.target as HTMLImageElement)}
  },{rootMargin:'300px 0px'});
  let eager=0;
  root.querySelectorAll<HTMLImageElement>('[data-cover-image]').forEach(image=>{
    // Above-the-fold cards begin now, not after browser lazy-load heuristics.
    // Lower rows use the same queue as they approach the viewport.
    if(eager++<10||!observer)start(image);else observer.observe(image);
  });
}

export function coverPlaceholder(appId:number){return`data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' width='460' height='215' data-appid='${appId}'/%3E`}
