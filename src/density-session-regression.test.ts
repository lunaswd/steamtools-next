// @vitest-environment happy-dom
import { beforeEach, expect, it, vi } from 'vitest';
import type { AchievementCatalog, AchievementChange } from './types';

const api=vi.hoisted(()=>Object.fromEntries([
  'getEnvironment','loadSettings','scanSteam','scanLuaLibrary',
  'detectKernelInstallation','searchCatalog','loadGameCover','loadAchievementImage',
  'listAchievements','applyAchievementChanges','loadCatalogRankings','getCurrentSteamSession',
].map(name=>[name,vi.fn()])));
vi.mock('./api',()=>api);
const STEAM='C:\\Fixture\\Steam';
const entry=(id:string,displayName:string,unlocked:boolean|null,protectedFlag=false)=>({id,displayName,description:'Fixture description',unlocked,protected:protectedFlag,hidden:false,unlockTime:null,icon:'',iconGray:''});
const catalog:AchievementCatalog={appId:42,requestedLanguage:'english',resolvedLanguage:'english',source:'steam-client',connected:true,writable:true,error:null,achievements:[entry('LOCK_A','Locked A',false),entry('LOCK_B','Locked B',false),entry('OPEN_A','Unlocked A',true),entry('PROTECTED','Protected',false,true),entry('UNKNOWN','Unknown',null)]};
function element<T extends Element=HTMLElement>(selector:string):T{const result=document.querySelector<T>(selector);expect(result,selector).not.toBeNull();return result!}
function click(selector:string){element<HTMLElement>(selector).click()}
function input(selector:string,value:string){const field=element<HTMLInputElement>(selector);field.value=value;field.dispatchEvent(new InputEvent('input',{bubbles:true,data:value}))}
async function settled(){await vi.waitFor(()=>expect(document.querySelector('.progress-strip')).toBeNull(),{timeout:3000,interval:5})}
async function openGame(appId=42){click(`[data-action="achievement-select:${appId}"]`);await vi.waitFor(()=>expect(document.querySelectorAll('.achievement-row')).toHaveLength(5))}
function assertWritesDisabled(){
  for(const action of ['achievement-unlock-all','achievement-lock-all','achievement-unlock-selected','achievement-lock-selected'])expect(element<HTMLButtonElement>(`[data-action="${action}"]`).disabled,action).toBe(true);
  for(const field of document.querySelectorAll<HTMLInputElement>('[data-achievement-id]'))expect(field.disabled,field.dataset.achievementId).toBe(true);
}

beforeEach(async()=>{
  vi.resetModules();Object.values(api).forEach(mock=>mock.mockReset());localStorage.clear();
  document.body.innerHTML='<div id="app"></div>';history.replaceState(null,'','#kernel');
  Object.defineProperty(window,'matchMedia',{configurable:true,value:()=>({matches:true,addEventListener:vi.fn()})});
  Object.defineProperty(window,'confirm',{configurable:true,value:vi.fn(()=>true)});
  HTMLElement.prototype.scrollTo=vi.fn();HTMLElement.prototype.scrollIntoView=vi.fn();
  api.getEnvironment.mockResolvedValue({locale:'en',version:'0.2.3',desktop:true});
  api.loadSettings.mockResolvedValue({language:'en',kernelPath:null,steamPath:STEAM});
  api.scanSteam.mockResolvedValue({capturedAt:1,steam:{path:STEAM},libraries:[],apps:[],warnings:[]});
  api.scanLuaLibrary.mockResolvedValue({layout:{},entries:[42,43].map(appId=>({appId,fileName:`${appId}.lua`,enabled:true,appIds:[appId],pinnedManifestCount:0})),warnings:[]});
  api.detectKernelInstallation.mockResolvedValue(null);
  api.searchCatalog.mockResolvedValue([]);api.loadGameCover.mockResolvedValue(null);api.loadAchievementImage.mockResolvedValue(null);
  api.loadCatalogRankings.mockResolvedValue({popular:[],recommended:[]});
  api.getCurrentSteamSession.mockResolvedValue({connected:false,source:'steamClient',accountLabel:'',accountFingerprint:'',steamPath:STEAM,games:[],capabilities:{},warnings:[],error:'Fixture session offline'});
  api.listAchievements.mockImplementation(async(_path,appId)=>({...structuredClone(catalog),appId}));
  api.applyAchievementChanges.mockImplementation(async(_path,appId,_locale,changes:AchievementChange[])=>({appId,confirmed:true,serverAcknowledged:true,localVerified:true,results:changes.map(change=>({...change,success:true,localVerified:true,error:null})),catalog:{...structuredClone(catalog),appId,achievements:catalog.achievements.map(item=>({...item,unlocked:changes.find(change=>change.id===item.id)?.unlock??item.unlocked}))}}));
  await import('./main');await settled();click('[data-page="achievements"]');await openGame();
});

it('invalidates cached writable state after an offline apply error until manual refresh, without auto-retrying a write',async()=>{
  api.applyAchievementChanges.mockRejectedValueOnce({code:'steam_session_offline',messageKey:'error.achievementApplyFailed',messageArgs:{},detail:'Fixture session offline or signed out'});
  click('[data-action="achievement-unlock-all"]');await settled();
  expect(element('.achievement-inline-error').textContent).toContain('Fixture session offline');
  assertWritesDisabled();
  expect(element('[data-achievement-row="LOCK_A"]').textContent).toContain('Unknown state');
  expect(document.querySelector('.achievement-selected-header')?.textContent).not.toContain('Steam connected');
  click('[data-action="achievement-unlock-all"]');await new Promise(resolve=>setTimeout(resolve,20));
  expect(api.applyAchievementChanges).toHaveBeenCalledTimes(1);
  expect(localStorage.getItem('stn-achievement-confirmed-v1')).toBeNull();
  click('[data-action="achievement-reload"]');await vi.waitFor(()=>expect(element<HTMLButtonElement>('[data-action="achievement-unlock-all"]').disabled).toBe(false));
  expect(api.applyAchievementChanges).toHaveBeenCalledTimes(1);
});

it('does not label an empty failed offline catalog as a game having no achievements',async()=>{
  api.listAchievements.mockResolvedValueOnce({...catalog,connected:false,writable:false,error:'Fixture offline schema unavailable',achievements:[]});
  click('[data-action="achievement-reload"]');
  await vi.waitFor(()=>expect(document.querySelector('.achievement-adapter-message')?.textContent).toContain('Fixture offline schema unavailable'));
  expect(element('.achievement-tab-scroll').textContent).not.toContain('No achievements defined');
  expect(element('[data-achievement-tab="42"]').textContent).not.toContain('No achievements');
  assertWritesDisabled();expect(api.applyAchievementChanges).not.toHaveBeenCalled();
});

it('does not leave another cached game tab writable after a global session-readiness failure',async()=>{
  click('[data-achievement-tab="library"]');await openGame(43);click('[data-achievement-tab="42"]');
  api.applyAchievementChanges.mockRejectedValueOnce({code:'steam_session_offline',messageKey:'error.achievementApplyFailed',messageArgs:{},detail:'Fixture session offline or signed out'});
  // A read-only recheck may run, but the fixture remains offline and never
  // permits an automatic write attempt in the other cached game context.
  api.listAchievements.mockImplementation(async(_path,appId)=>({...structuredClone(catalog),appId,connected:false,writable:false,sessionErrorCode:'steam_session_offline',error:'Fixture session offline',achievements:catalog.achievements.map(item=>({...item,unlocked:null,unlockTime:null}))}));
  click('[data-action="achievement-unlock-all"]');await settled();click('[data-achievement-tab="43"]');
  await new Promise(resolve=>setTimeout(resolve,20));assertWritesDisabled();
  click('[data-action="achievement-unlock-all"]');expect(api.applyAchievementChanges).toHaveBeenCalledTimes(1);
  click('[data-achievement-close="43"]');click('[data-achievement-tab="library"]');await openGame(43);
  assertWritesDisabled();expect(api.applyAchievementChanges).toHaveBeenCalledTimes(1);
  api.listAchievements.mockResolvedValueOnce({...structuredClone(catalog),appId:43});
  click('[data-action="achievement-reload"]');
  await vi.waitFor(()=>expect(element<HTMLButtonElement>('[data-action="achievement-unlock-all"]').disabled).toBe(false));
  expect(api.applyAchievementChanges).toHaveBeenCalledTimes(1);
});

it('unlocks only known editable locked rows, never protected or unknown rows',async()=>{
  click('[data-action="achievement-unlock-all"]');await settled();
  expect(api.applyAchievementChanges).toHaveBeenCalledWith(STEAM,42,'en',[{id:'LOCK_A',unlock:true},{id:'LOCK_B',unlock:true}]);
});

it('relocks only known editable unlocked rows, never protected or unknown rows',async()=>{
  click('[data-action="achievement-lock-all"]');await settled();
  expect(api.applyAchievementChanges).toHaveBeenCalledWith(STEAM,42,'en',[{id:'OPEN_A',unlock:false}]);
});

it('keeps explicit checked selection across filters and submits only those IDs',async()=>{
  input('#achievement-detail-query','Locked A');click('[data-action="achievement-select-visible"]');
  input('#achievement-detail-query','Locked B');click('[data-action="achievement-select-visible"]');
  input('#achievement-detail-query','Protected');
  expect(element<HTMLInputElement>('[data-achievement-id="PROTECTED"]').disabled).toBe(true);
  click('[data-action="achievement-unlock-selected"]');await settled();
  expect(api.applyAchievementChanges).toHaveBeenCalledWith(STEAM,42,'en',[{id:'LOCK_A',unlock:true},{id:'LOCK_B',unlock:true}]);
});

it('uses one unified heading instead of a second vertical page-head on every page',async()=>{
  for(const page of ['kernel','library','import','manifest','extract','achievements','overview','diagnostics','settings']){
    click(`[data-page="${page}"]`);await settled();
    expect(document.querySelector('main > .page-head'),page).toBeNull();
    expect(document.querySelectorAll('h1'),page).toHaveLength(1);
  }
  expect(api.applyAchievementChanges).not.toHaveBeenCalled();
});
