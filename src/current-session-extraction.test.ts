// @vitest-environment happy-dom
import { beforeEach, expect, it, vi } from 'vitest';
import type { CurrentSteamSession, CurrentSessionExtraction, ExtractionExportReceipt } from './types';

const api=vi.hoisted(()=>Object.fromEntries([
  'getEnvironment','loadSettings','scanSteam','scanLuaLibrary',
  'detectKernelInstallation','searchCatalog','loadGameCover','loadAchievementImage',
  'getCurrentSteamSession','extractCurrentSession','exportCurrentSession','chooseDirectory',
  'addLuaEntry','catalogAdd','importLibraryFiles','setLuaEntryEnabled','applyAchievementChanges',
].map(name=>[name,vi.fn()])));
vi.mock('./api',()=>api);
const STEAM='C:\\Fixture\\Steam',DESTINATION='C:\\Fixture\\Export';
const session:CurrentSteamSession={connected:true,source:'steamClient',accountLabel:'Fixture account',accountFingerprint:'fixture-account-a',steamPath:STEAM,games:[{appId:42,name:'Fixture Game',installed:true,subscribed:true}],capabilities:{subscriptionChecks:true,installedDepotKeys:true,freshDepotKeyRequests:false,localManifests:true,achievementSchema:true},warnings:[],error:null};
const extraction:CurrentSessionExtraction={session,appId:42,appName:'Fixture Game',subscribed:true,scope:'installedDepots',depots:[{depotId:4200,manifestId:'420001',keyAvailable:true,keySource:'steamLocalConfig',missingReason:null},{depotId:4201,manifestId:null,keyAvailable:false,keySource:null,missingReason:'Fixture missing cached key'}],keyedDepotCount:1,lua:'-- TEST_KEY is only a dummy fixture marker\naddappid(42)\n',assets:[{kind:'achievementSchema',filename:'UserGameStatsSchema_42.bin',sourcePath:`${STEAM}\\appcache\\stats\\UserGameStatsSchema_42.bin`,sizeBytes:64,sha256:'fixture-hash'}],complete:false,warnings:['Fixture local data only']};
const exported:ExtractionExportReceipt={appId:42,destinationDirectory:DESTINATION,files:[{name:'42.lua',kind:'lua',sizeBytes:64,sha256:'fixture-hash'},{name:'UserGameStatsSchema_42.bin',kind:'achievementSchema',sizeBytes:64,sha256:'fixture-hash'}],keyedDepotCount:1,warnings:[]};
function element<T extends Element=HTMLElement>(selector:string):T{const result=document.querySelector<T>(selector);expect(result,selector).not.toBeNull();return result!}
function click(selector:string){element<HTMLElement>(selector).click()}
async function settled(){await vi.waitFor(()=>expect(document.querySelector('.progress-strip')).toBeNull(),{timeout:3000,interval:5})}
function noSteamWrites(){for(const name of ['addLuaEntry','catalogAdd','importLibraryFiles','setLuaEntryEnabled','applyAchievementChanges'])expect(api[name],name).not.toHaveBeenCalled()}
function chooseGame(){const select=element<HTMLSelectElement>('#extract-session-game');select.value='42';select.dispatchEvent(new Event('change',{bubbles:true}))}

beforeEach(async()=>{
  vi.resetModules();Object.values(api).forEach(mock=>mock.mockReset());localStorage.clear();
  document.body.innerHTML='<div id="app"></div>';history.replaceState(null,'','#kernel');
  Object.defineProperty(window,'matchMedia',{configurable:true,value:()=>({matches:true,addEventListener:vi.fn()})});
  HTMLElement.prototype.scrollTo=vi.fn();HTMLElement.prototype.scrollIntoView=vi.fn();
  api.getEnvironment.mockResolvedValue({locale:'en',version:'0.2.3',desktop:true});
  api.loadSettings.mockResolvedValue({language:'en',kernelPath:null,steamPath:STEAM});
  api.scanSteam.mockResolvedValue({capturedAt:1,steam:{path:STEAM},apps:[],warnings:[]});
  api.scanLuaLibrary.mockResolvedValue({layout:{},entries:[],warnings:[]});
  api.detectKernelInstallation.mockResolvedValue(null);
  api.loadGameCover.mockResolvedValue(null);api.loadAchievementImage.mockResolvedValue(null);
  api.getCurrentSteamSession.mockResolvedValue(structuredClone(session));
  api.extractCurrentSession.mockResolvedValue(structuredClone(extraction));
  api.chooseDirectory.mockResolvedValue(null);api.exportCurrentSession.mockResolvedValue(structuredClone(exported));
  await import('./main');await settled();click('[data-page="extract"]');
  await vi.waitFor(()=>expect(element('.current-session-account').textContent).toContain('Fixture account'));
});

it('connects the current-session adapter without asking for credentials or writing to Steam',async()=>{
  expect(api.getCurrentSteamSession).toHaveBeenCalledWith(STEAM);
  expect(element('.current-session-panel').textContent).toContain('does not request fresh keys');
  expect(element('.current-session-panel').textContent).toContain('complete owned library');
  expect(element('.current-session-panel').textContent).not.toContain('fixture-account-a');
  expect(document.querySelector('input[type="password"]')).toBeNull();
  expect(api.extractCurrentSession).not.toHaveBeenCalled();expect(api.exportCurrentSession).not.toHaveBeenCalled();noSteamWrites();
  click('[data-action="session-connect"]');await vi.waitFor(()=>expect(api.getCurrentSteamSession).toHaveBeenCalledTimes(2));
  noSteamWrites();
});

it('reads only the selected game with the current account fingerprint and labels missing cached keys',async()=>{
  chooseGame();click('[data-action="session-extract"]');
  await vi.waitFor(()=>expect(api.extractCurrentSession).toHaveBeenCalledWith(STEAM,42,'en','fixture-account-a'));await settled();
  expect(element('.extraction-result').textContent).toContain('Partial local data');
  expect(element('.extraction-result').textContent).toContain('No usable cached key');
  expect(element('.extraction-result').textContent).toContain('UserGameStatsSchema_42.bin');
  expect(element<HTMLDetailsElement>('.extraction-lua-preview').open).toBe(false);
  expect(element('.current-session-panel').textContent).not.toContain('TEST_KEY');
  expect(api.exportCurrentSession).not.toHaveBeenCalled();noSteamWrites();
});

it('exports assets only after the user explicitly selects a destination directory',async()=>{
  chooseGame();click('[data-action="session-extract"]');await vi.waitFor(()=>expect(document.querySelector('.extraction-result')).not.toBeNull());await settled();
  click('[data-action="session-export"]');await vi.waitFor(()=>expect(api.chooseDirectory).toHaveBeenCalledTimes(1));
  expect(api.exportCurrentSession).not.toHaveBeenCalled();
  api.chooseDirectory.mockResolvedValueOnce(DESTINATION);click('[data-action="session-export"]');
  await vi.waitFor(()=>expect(api.exportCurrentSession).toHaveBeenCalledWith(STEAM,42,'en','fixture-account-a',DESTINATION));await settled();
  expect(document.querySelector('.page-content')?.textContent).toContain(DESTINATION);noSteamWrites();
});

it('rejects a changed-account extraction result instead of exposing an export action',async()=>{
  api.extractCurrentSession.mockResolvedValueOnce({...structuredClone(extraction),session:{...session,accountFingerprint:'fixture-account-b'}});
  chooseGame();click('[data-action="session-extract"]');
  await vi.waitFor(()=>expect(document.querySelector('.current-session-panel .error-banner')?.textContent).toContain('account changed'));await settled();
  expect(document.querySelector('.extraction-result')).toBeNull();
  expect(document.querySelector('[data-action="session-export"]')).toBeNull();expect(api.exportCurrentSession).not.toHaveBeenCalled();noSteamWrites();
});

it('keeps read controls disabled after the current account signs out',async()=>{
  api.getCurrentSteamSession.mockResolvedValueOnce({...session,connected:false,accountLabel:'',accountFingerprint:'',games:[],error:'Fixture signed out'});
  click('[data-action="session-connect"]');
  await vi.waitFor(()=>expect(element('.current-session-panel').textContent).toContain('Current session not connected'));
  expect(element<HTMLButtonElement>('[data-action="session-extract"]').disabled).toBe(true);
  expect(element<HTMLSelectElement>('#extract-session-game').disabled).toBe(true);
  expect(api.extractCurrentSession).not.toHaveBeenCalled();expect(api.exportCurrentSession).not.toHaveBeenCalled();noSteamWrites();
});
