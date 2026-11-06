// @vitest-environment happy-dom
import { beforeEach, expect, it, vi } from 'vitest';
import type { CurrentSteamSession, CurrentSessionExtraction, SteamSnapshot } from './types';

const api=vi.hoisted(()=>Object.fromEntries([
  'getEnvironment','loadSettings','scanSteam','scanLuaLibrary','detectKernelInstallation',
  'searchCatalog','loadGameCover','loadAchievementImage','getCurrentSteamSession',
  'extractCurrentSession','exportCurrentSession','chooseDirectory',
].map(name=>[name,vi.fn()])));
vi.mock('./api',()=>api);
const STEAM='C:\\Fixture\\Steam';
const session:CurrentSteamSession={connected:true,source:'steamClient',accountLabel:'Fixture account',accountFingerprint:'fixture-account',steamPath:STEAM,games:[{appId:42,name:'Fixture Game',installed:true,subscribed:true}],capabilities:{subscriptionChecks:true,installedDepotKeys:true,freshDepotKeyRequests:false,localManifests:true,achievementSchema:true},warnings:[],error:null};
const extraction:CurrentSessionExtraction={session,appId:42,appName:'Fixture Game',subscribed:true,scope:'installedDepots',depots:[],keyedDepotCount:0,lua:'-- fixture',assets:[],complete:false,warnings:[]};
let snapshot:SteamSnapshot;
function element<T extends Element=HTMLElement>(selector:string):T{const e=document.querySelector<T>(selector);expect(e,selector).not.toBeNull();return e!}
function click(selector:string){element<HTMLElement>(selector).click()}
function selectGame(){const e=element<HTMLSelectElement>('#extract-session-game');e.value='42';e.dispatchEvent(new Event('change',{bubbles:true}))}
function deferred<T>(){let resolve!:(value:T)=>void;const promise=new Promise<T>(r=>{resolve=r});return {promise,resolve}}
async function boot(running:boolean,hash='#extract'){
  snapshot={capturedAt:1,locale:'en',steam:{path:STEAM,executable:`${STEAM}\\steam.exe`,version:null,clientBuild:null,running,detectionSource:'fixture'},libraries:[],apps:[],warnings:[]};
  api.scanSteam.mockResolvedValue(snapshot);history.replaceState(null,'',hash);await import('./main');
  await vi.waitFor(()=>expect(document.querySelector('.progress-strip')).toBeNull());
  await vi.waitFor(()=>expect(document.querySelector('.current-session-panel')).not.toBeNull());
}
async function connected(){await boot(true);await vi.waitFor(()=>expect(element('.current-session-account').textContent).toContain('Fixture account'))}
async function read(){selectGame();click('[data-action="session-extract"]');await vi.waitFor(()=>expect(document.querySelector('.extraction-result')).not.toBeNull())}
async function assertOffline(){
  await vi.waitFor(()=>expect(element('.current-session-panel').textContent).toContain('Start Steam'));
  expect(element('.current-session-account').textContent).not.toContain('Fixture account');
  expect(element<HTMLSelectElement>('#extract-session-game').disabled).toBe(true);
  expect(element<HTMLButtonElement>('[data-action="session-extract"]').disabled).toBe(true);
  expect(element('.current-session-panel').textContent).not.toContain('Connecting to current Steam');
  expect(document.querySelector('.extraction-result')).toBeNull();
  expect(document.querySelector('[data-action="session-export"]')).toBeNull();
}
beforeEach(()=>{
  vi.resetModules();Object.values(api).forEach(mock=>mock.mockReset());localStorage.clear();document.body.innerHTML='<div id="app"></div>';
  Object.defineProperty(window,'matchMedia',{configurable:true,value:()=>({matches:true,addEventListener:vi.fn()})});
  HTMLElement.prototype.scrollTo=vi.fn();HTMLElement.prototype.scrollIntoView=vi.fn();
  api.getEnvironment.mockResolvedValue({locale:'en',version:'0.3.0',desktop:true});
  api.loadSettings.mockResolvedValue({language:'en',steamPath:STEAM});api.scanLuaLibrary.mockResolvedValue({layout:{},entries:[],warnings:[]});
  api.detectKernelInstallation.mockResolvedValue(null);api.loadGameCover.mockResolvedValue(null);api.loadAchievementImage.mockResolvedValue(null);
  api.getCurrentSteamSession.mockResolvedValue(structuredClone(session));api.extractCurrentSession.mockResolvedValue(structuredClone(extraction));
  api.chooseDirectory.mockResolvedValue(null);
});

it('shows stopped-client guidance without invoking the native session worker on entry or connect',async()=>{
  await boot(false);await assertOffline();expect(api.getCurrentSteamSession).not.toHaveBeenCalled();
  click('[data-action="session-connect"]');await assertOffline();expect(api.getCurrentSteamSession).not.toHaveBeenCalled();
  await vi.waitFor(()=>expect(element<HTMLButtonElement>('[data-action="extract-generate"]').disabled).toBe(false));
});
it('invalidates stale account and extraction state when the stopped snapshot is encountered before read',async()=>{
  await connected();selectGame();snapshot.steam!.running=false;click('[data-action="session-extract"]');await assertOffline();
  expect(api.extractCurrentSession).not.toHaveBeenCalled();expect(api.exportCurrentSession).not.toHaveBeenCalled();
});
it('does not open a destination picker or export when Steam stopped before export',async()=>{
  await connected();await read();snapshot.steam!.running=false;click('[data-action="session-export"]');await assertOffline();
  expect(api.chooseDirectory).not.toHaveBeenCalled();expect(api.exportCurrentSession).not.toHaveBeenCalled();
});
it('rechecks stopped state after the destination picker before calling native export',async()=>{
  await connected();await read();const picker=deferred<string>();api.chooseDirectory.mockReturnValueOnce(picker.promise);
  click('[data-action="session-export"]');await vi.waitFor(()=>expect(api.chooseDirectory).toHaveBeenCalledTimes(1));
  snapshot.steam!.running=false;picker.resolve('C:\\Fixture\\Export');await assertOffline();expect(api.exportCurrentSession).not.toHaveBeenCalled();
});
it('clears cached connected state after a backend offline read instead of enabling another read',async()=>{
  await connected();api.extractCurrentSession.mockRejectedValueOnce({code:'steam_session_offline',messageKey:'error.steamSessionOffline',detail:'Fixture stopped'});
  selectGame();click('[data-action="session-extract"]');await assertOffline();
  click('[data-action="session-extract"]');expect(api.extractCurrentSession).toHaveBeenCalledTimes(1);
});
it('discards a successful read reply if the known client state changed to stopped in flight',async()=>{
  await connected();const pending=deferred<CurrentSessionExtraction>();api.extractCurrentSession.mockReturnValueOnce(pending.promise);
  selectGame();click('[data-action="session-extract"]');await vi.waitFor(()=>expect(api.extractCurrentSession).toHaveBeenCalledTimes(1));
  snapshot.steam!.running=false;pending.resolve(structuredClone(extraction));await assertOffline();
});
it('clears stale state after a typed invalid-client reply and displays safe repair guidance',async()=>{
  await connected();await read();api.getCurrentSteamSession.mockRejectedValueOnce({code:'steam_client_invalid',messageKey:'error.steamClientInvalid',detail:''});
  click('[data-action="session-connect"]');
  await vi.waitFor(()=>expect(element('.current-session-panel').textContent).toContain('Select the real Steam folder'));
  expect(element('.current-session-account').textContent).not.toContain('Fixture account');
  expect(document.querySelector('.extraction-result')).toBeNull();expect(api.exportCurrentSession).not.toHaveBeenCalled();
});
it('refreshes and reconnects after Steam is started, without replaying previous extraction',async()=>{
  await boot(false);await assertOffline();snapshot.steam!.running=true;click('[data-action="refresh"]');
  await vi.waitFor(()=>expect(element('.current-session-account').textContent).toContain('Fixture account'));
  expect(api.getCurrentSteamSession).toHaveBeenCalledTimes(1);expect(api.extractCurrentSession).not.toHaveBeenCalled();
  selectGame();click('[data-action="session-extract"]');await vi.waitFor(()=>expect(api.extractCurrentSession).toHaveBeenCalledTimes(1));
});
it('rescans and reconnects from the session button after Steam starts while the old snapshot remains stopped',async()=>{
  await boot(false);await assertOffline();
  const runningSnapshot:SteamSnapshot={...snapshot,steam:{...snapshot.steam!,running:true}};
  api.scanSteam.mockResolvedValueOnce(runningSnapshot);
  click('[data-action="session-connect"]');
  await vi.waitFor(()=>expect(element('.current-session-account').textContent).toContain('Fixture account'));
  expect(api.scanSteam).toHaveBeenCalledTimes(2);
  expect(snapshot.steam!.running).toBe(false);
  expect(api.getCurrentSteamSession).toHaveBeenCalledTimes(1);
  expect(api.extractCurrentSession).not.toHaveBeenCalled();
});
it('does not adopt a late connected worker reply after a refresh detects the client stopped',async()=>{
  const pending=deferred<CurrentSteamSession>();api.getCurrentSteamSession.mockReturnValueOnce(pending.promise);await boot(true);
  snapshot.steam!.running=false;click('[data-action="refresh"]');await assertOffline();pending.resolve(structuredClone(session));
  await new Promise(resolve=>setTimeout(resolve,0));await assertOffline();expect(api.getCurrentSteamSession).toHaveBeenCalledTimes(1);
});
