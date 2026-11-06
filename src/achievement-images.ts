import { loadAchievementImage } from './api';
import { browserImage, CoverLoader, coverPlaceholder } from './covers';
import type { CoverRequest } from './covers';
import type { GameCover } from './types';

function imageName(appId:number,raw:string){
  try{
    let name=raw.trim();
    if(name.includes('://')){
      const url=new URL(name),prefix=`/community_assets/images/apps/${appId}/`,legacy=`/steamcommunity/public/images/apps/${appId}/`;
      if(url.protocol!=='https:'||url.username||url.password||(!url.hostname.endsWith('.steamstatic.com')&&url.hostname!=='steamstatic.com'&&url.hostname!=='steamcdn-a.akamaihd.net'))return'';
      name=url.pathname.startsWith(prefix)?url.pathname.slice(prefix.length):url.pathname.startsWith(legacy)?url.pathname.slice(legacy.length):'';
    }
    return /^[a-zA-Z0-9][a-zA-Z0-9._-]{0,127}$/.test(name)&&!name.includes('..')?name:'';
  }catch{return''}
}
export function achievementImageCandidates(appId:number,image:string){
  const name=imageName(appId,image);if(!name)return[];
  return[
    `https://shared.akamai.steamstatic.com/community_assets/images/apps/${appId}/${name}`,
    `https://shared.fastly.steamstatic.com/community_assets/images/apps/${appId}/${name}`,
    `https://cdn.cloudflare.steamstatic.com/community_assets/images/apps/${appId}/${name}`,
    `https://cdn.akamai.steamstatic.com/steamcommunity/public/images/apps/${appId}/${name}`,
    `https://cdn.steamstatic.com/steamcommunity/public/images/apps/${appId}/${name}`,
  ];
}
const iconRequest=(appId:number,image:string,fallback:string,desktop:boolean):CoverRequest=>({steamPath:null,appId,language:'icon',resourceId:`achievement:${imageName(appId,image)}:${imageName(appId,fallback)}`,candidates:[image,fallback],desktop});
async function browserAchievement(request:CoverRequest):Promise<GameCover|null>{
  const primary=achievementImageCandidates(request.appId,request.candidates[0]),fallback=achievementImageCandidates(request.appId,request.candidates[1]||'');
  const deadline=Date.now()+6000;
  for(let index=0;index<primary.length;index++)for(const [isFallback,url]of [[false,primary[index]],[true,fallback[index]]] as const){
    const remaining=deadline-Date.now();if(remaining<=0)return null;
    if(url&&await browserImage(url,Math.min(2500,remaining)))return{appId:request.appId,dataUrl:url,source:isFallback?'browserColorFallback':'browserCache'};
  }
  return null;
}
export const achievementImageLoader=new CoverLoader(request=>request.desktop?loadAchievementImage(request.appId,request.candidates[0],request.candidates[1]||null):browserAchievement(request),6,8*1024*1024);
const cachedIcons=new CoverLoader(request=>request.desktop?loadAchievementImage(request.appId,request.candidates[0],null,true):Promise.resolve(null),12,8*1024*1024);
export function achievementImageMarkup(appId:number,image:string,fallback:string,desktop:boolean){
  if(!imageName(appId,image))return'';
  const cached=achievementImageLoader.peek(iconRequest(appId,image,fallback,desktop));
  return`<img src="${cached?.dataUrl||coverPlaceholder(appId)}" alt="" decoding="async" width="64" height="64" data-achievement-image data-icon-appid="${appId}" data-icon-image="${encodeURIComponent(image)}" data-icon-fallback="${encodeURIComponent(fallback)}">`;
}
let observer:IntersectionObserver|null=null;
export function bindAchievementImages(root:HTMLElement,desktop:boolean){
  observer?.disconnect();observer=null;
  const start=(image:HTMLImageElement)=>{
    const request=iconRequest(Number(image.dataset.iconAppid),decodeURIComponent(image.dataset.iconImage||''),decodeURIComponent(image.dataset.iconFallback||''),desktop);
    const set=(cover:GameCover|null)=>{if(!image.isConnected)return;if(cover){image.src=cover.dataUrl;image.dataset.iconSource=cover.source;image.classList.toggle('color-fallback',cover.source.includes('ColorFallback'));image.parentElement?.classList.remove('missing')}else image.parentElement?.classList.add('missing')};
    const cached=achievementImageLoader.peek(request);if(cached){set(cached);return}
    void cachedIcons.load(request).then(local=>{if(local){achievementImageLoader.remember(request,local);set(local)}else void achievementImageLoader.load(request).then(set)});
  };
  if(typeof IntersectionObserver!=='undefined')observer=new IntersectionObserver(entries=>{for(const entry of entries)if(entry.isIntersecting){observer?.unobserve(entry.target);start(entry.target as HTMLImageElement)}},{rootMargin:'180px 0px'});
  root.querySelectorAll<HTMLImageElement>('[data-achievement-image]').forEach(image=>{if(observer)observer.observe(image);else start(image)});
}
