import { invoke, isTauri } from '@tauri-apps/api/core';
import { open } from '@tauri-apps/plugin-dialog';
import type {
  ArchiveImportSummary, CatalogAddReceipt, CatalogGame, CatalogRankings, Environment, ImportSummary,
  KernelInstallation, KernelOperationPlan, RemoteKernelInstallRequest, RemoteKernelInstallReceipt, KernelReleaseArtifact,
  KernelOperationReceipt,
  KernelUninstallRequest, KeyCacheUpdate, LuaGenerationOptions, LuaLibrarySnapshot,
  LuaMutationReceipt, ManagedWrite, ManifestLogCursor, ManifestPollResponse, RemoteImportResult,
  Settings, SteamSnapshot,
  AchievementCatalog, AchievementChange, AchievementApplyResult,
} from './types';
import { DEFAULT_KERNEL_RELEASE_SOURCE, migrateKernelSettings } from './remote-kernel';

const desktop = isTauri();
const defaults: Settings = {
  language:'auto', theme:'dark', accentColor:'steamBlue', compactMode:false, showCovers:true, reduceMotion:false, steamPath:null, kernelPath:null, kernelArtifactPath:null, kernelReleaseSource:DEFAULT_KERNEL_RELEASE_SOURCE,
  injectionMethod:'dwmapi', downloadMode:'depotKeys', includeDlc:true, pinManifest:false,
  fetchNameComments:false, autoRefreshKeyCache:true, showRanking:true,
  autoRefreshLibrary:true, enableLogging:true,
};

const demoSteam: SteamSnapshot = {
  capturedAt:Math.floor(Date.now()/1000), locale:navigator.language,
  steam:{path:'C:\\Demo\\Steam',executable:'C:\\Demo\\Steam\\steam.exe',version:'Demo',clientBuild:null,running:false,detectionSource:'demo'},
  libraries:[{path:'C:\\Demo\\Steam',exists:true,appCount:3,sizeBytes:34078720000}],
  apps:[
    {appid:101,name:'Example Adventure',installDir:'C:\\Demo\\Steam\\steamapps\\common\\Example Adventure',manifestPath:'demo',libraryPath:'C:\\Demo\\Steam',sizeBytes:21474836480,stateFlags:4,lastUpdated:0},
    {appid:102,name:'Sample Workshop',installDir:'C:\\Demo\\Steam\\steamapps\\common\\Sample Workshop',manifestPath:'demo',libraryPath:'C:\\Demo\\Steam',sizeBytes:10737418240,stateFlags:4,lastUpdated:0},
    {appid:103,name:'Demo Puzzle',installDir:'C:\\Demo\\Steam\\steamapps\\common\\Demo Puzzle',manifestPath:'demo',libraryPath:'C:\\Demo\\Steam',sizeBytes:1866465280,stateFlags:6,lastUpdated:0},
  ], warnings:[],
};
let demoKernelInstallation: KernelInstallation = {
  path:'C:\\Demo\\Steam',status:'notInstalled',injectionMethod:'none',version:null,
  versionSource:'unknown',installedSha256:null,artifactVersion:null,artifactSha256:null,hashMatch:null,runtimeLoadVerified:false,
  files:[],remnants:[],messageKey:'diagnostic.kernel_installation.not_installed.body',
  messageArgs:{path:'C:\\Demo\\Steam',injectionMethod:'none'},diagnostics:[],
};
let demoLua: LuaLibrarySnapshot = {
  layout:{steamRoot:'C:\\Demo\\Steam',luaRoot:'C:\\Demo\\Steam\\config\\lua',disabledRoot:'C:\\Demo\\Steam\\config\\lua\\Disable',configuredPaths:[],source:'default',warnings:[]},
  entries:[
    {appId:900000101,fileName:'900000101.lua',enabled:true,conflict:false,activePath:'C:\\Demo\\Steam\\config\\lua\\900000101.lua',disabledPath:null,sizeBytes:128,sha256:'demo-900000101',appIds:[900000101],pinnedManifestCount:0,pinnedDepotIds:[]},
    {appId:900000102,fileName:'900000102.lua',enabled:false,conflict:false,activePath:null,disabledPath:'C:\\Demo\\Steam\\config\\lua\\Disable\\900000102.lua',sizeBytes:96,sha256:'demo-900000102',appIds:[900000102],pinnedManifestCount:1,pinnedDepotIds:[900000103]},
  ], warnings:[],
};
const demoCatalog: CatalogGame[] = [
  {appId:900000201,name:'示例冒险',coverUrl:'',coverCandidates:[],releaseDate:'',followers:1000,source:'demo'},
  {appId:900000202,name:'示例工坊',coverUrl:'',coverCandidates:[],releaseDate:'',followers:2000,source:'demo'},
  {appId:900000203,name:'示例谜题',coverUrl:'',coverCandidates:[],releaseDate:'',followers:3000,source:'demo'},
  {appId:900000204,name:'示例探索',coverUrl:'',coverCandidates:[],releaseDate:'',followers:4000,source:'demo'},
];

const clone = <T>(value:T):T => structuredClone(value);
const artifactVersionFromPath = (path:string):string|null => path.match(/opensteamtool[-_](\d+(?:\.\d+){1,3})/i)?.[1]||null;
const demoManagedWrite = (appId:number,replaced:boolean):ManagedWrite => ({
  destination:`C:\\Demo\\Steam\\config\\lua\\${appId}.lua`,backupPath:replaced?`C:\\Demo\\Steam\\config\\lua\\.stn-backups\\${appId}.lua`:null,
  sizeBytes:48,sha256:`demo-${appId}`,replaced,
});
function demoPlan(request:RemoteKernelInstallRequest,release:KernelReleaseArtifact):KernelOperationPlan {
  return {operationId:'demo-plan',operation:'install',steamRoot:request.steamRoot,backupDir:`${request.steamRoot}\\.steamtools-next-backups\\demo`,
    previousState:clone(demoKernelInstallation),expectedStatus:'enabled',expectedInjectionMethod:'dwmapi',steamRunning:false,
    alreadySatisfied:demoKernelInstallation.status==='enabled'&&demoKernelInstallation.injectionMethod==='dwmapi',blockers:[],actions:[],artifactRoot:release.artifactRoot,artifactVersion:release.version,artifactSha256:'demo-artifact'};
}
function demoReceipt(operation:KernelOperationReceipt['operation'],plan:KernelOperationPlan,resultingState:KernelInstallation):KernelOperationReceipt {
  return {schemaVersion:1,operationId:plan.operationId,operation,dryRun:false,changed:true,steamRoot:plan.steamRoot,backupDir:plan.backupDir,
    receiptPath:`${plan.backupDir}\\receipt.json`,previousState:clone(demoKernelInstallation),resultingState:clone(resultingState),records:[],plan};
}

export async function getEnvironment():Promise<Environment> {
  return desktop ? invoke('get_environment_command') : {locale:navigator.language,version:'0.3.6',defaultKernelPath:null,defaultKernelArtifactPath:null,defaultKernelReleaseSource:DEFAULT_KERNEL_RELEASE_SOURCE,desktop:false};
}
export async function scanSteam(path?:string|null):Promise<SteamSnapshot> {
  return desktop ? invoke('scan_steam_command',{path:path||null}) : clone(demoSteam);
}
export async function detectKernelInstallation(steamPath:string,artifactPath?:string|null):Promise<KernelInstallation> {
  return desktop ? invoke('detect_kernel_installation_command',{steamPath,artifactPath:artifactPath||null}) : {...clone(demoKernelInstallation),path:steamPath,version:demoKernelInstallation.status==='enabled'?'1.5.4.1':null,versionSource:demoKernelInstallation.status==='enabled'?'artifactManifest':'unknown',artifactVersion:artifactVersionFromPath(artifactPath||'')||'1.5.4.1',hashMatch:demoKernelInstallation.status==='enabled'?true:null};
}

export async function scanLuaLibrary(steamPath:string):Promise<LuaLibrarySnapshot> {
  return desktop ? invoke('scan_lua_library_command',{steamPath}) : clone(demoLua);
}
export async function addLuaEntry(steamPath:string,appId:number,customLua:string|null=null,overwrite=true):Promise<ManagedWrite> {
  if(desktop) return invoke('add_lua_entry_command',{steamPath,appId,customLua,overwrite});
  const index=demoLua.entries.findIndex(entry=>entry.appId===appId); const replaced=index>=0;
  const entry={appId,fileName:`${appId}.lua`,enabled:true,conflict:false,activePath:`${steamPath}\\config\\lua\\${appId}.lua`,disabledPath:null,sizeBytes:customLua?.length||48,sha256:`demo-${appId}`,appIds:[appId],pinnedManifestCount:customLua?.includes('setManifestid')?1:0,pinnedDepotIds:[]};
  if(replaced)demoLua.entries[index]=entry;else demoLua.entries.push(entry);
  return demoManagedWrite(appId,replaced);
}
export async function importLibraryFiles(steamPath:string,sources:string[],overwrite=true):Promise<ImportSummary> {
  if(desktop) return invoke('import_library_files_command',{steamPath,sources,overwrite});
  return {luaCount:sources.filter(path=>path.toLowerCase().endsWith('.lua')).length,binCount:sources.filter(path=>path.toLowerCase().endsWith('.bin')).length,manifestCount:sources.filter(path=>path.toLowerCase().endsWith('.manifest')).length,imported:[],skipped:[],failed:[]};
}
export async function importLibraryArchives(steamPath:string,archives:string[],overwrite=true):Promise<ArchiveImportSummary> {
  if(desktop)return invoke('import_library_archives_command',{steamPath,archives,overwrite});
  const imported:ImportSummary={luaCount:archives.length,binCount:0,manifestCount:0,imported:[],skipped:[],failed:[]};
  return {archives:archives.map(archive=>({archive,format:archive.toLowerCase().endsWith('.zip')?'zip':archive.toLowerCase().endsWith('.gz')?'tarGz':'tar',extractedFiles:1,extractedBytes:128,success:true,error:null})),extractedFiles:archives.length,extractedBytes:archives.length*128,skipped:[],failed:[],import:imported};
}
export async function setLuaEntryEnabled(steamPath:string,appId:number,enabled:boolean,overwrite=true):Promise<LuaMutationReceipt> {
  if(desktop)return invoke('set_lua_entry_enabled_command',{steamPath,appId,enabled,overwrite});
  const entry=demoLua.entries.find(item=>item.appId===appId);if(entry){entry.enabled=enabled;entry.activePath=enabled?`${steamPath}\\config\\lua\\${appId}.lua`:null;entry.disabledPath=enabled?null:`${steamPath}\\config\\lua\\Disable\\${appId}.lua`;}
  return {appId,action:enabled?'enable':'disable',source:null,destination:entry?.activePath||entry?.disabledPath||null,backupPaths:[]};
}
export async function deleteLuaEntry(steamPath:string,appId:number):Promise<LuaMutationReceipt> {
  if(desktop)return invoke('delete_lua_entry_command',{steamPath,appId});
  demoLua.entries=demoLua.entries.filter(entry=>entry.appId!==appId);
  return {appId,action:'delete',source:null,destination:null,backupPaths:[`${steamPath}\\config\\lua\\.stn-backups\\${appId}.lua`]};
}

export async function searchCatalog(query:string,locale:string):Promise<CatalogGame[]> {
  if(desktop)return invoke('search_catalog_command',{query,locale});
  const needle=query.trim().toLocaleLowerCase(locale);const id=Number(query);
  return clone(demoCatalog.filter(game=>Number.isFinite(id)?game.appId===id:game.name.toLocaleLowerCase(locale).includes(needle)));
}
export async function loadCatalogRankings():Promise<CatalogRankings> {
  return desktop?invoke('catalog_rankings_command'):{popular:clone(demoCatalog),recommended:clone([...demoCatalog].reverse())};
}
export async function refreshKeyCache():Promise<KeyCacheUpdate> {
  return desktop?invoke('refresh_key_cache_command'):{cacheDir:'C:\\Demo\\cache\\catalog-v1',depotKeysPath:'C:\\Demo\\cache\\catalog-v1\\depotkeys.json',appTokensPath:'C:\\Demo\\cache\\catalog-v1\\appaccesstokens.json',depotKeysBytes:1024,appTokensBytes:512,updatedAt:Math.floor(Date.now()/1000)};
}
export async function catalogAdd(steamPath:string,appId:number,options:LuaGenerationOptions,overwrite=true):Promise<CatalogAddReceipt> {
  if(desktop)return invoke('catalog_add_command',{steamPath,appId,options,overwrite});
  const game=demoCatalog.find(item=>item.appId===appId);const lua=`-- Generated by SteamTools Next\r\naddappid(${appId})\r\n`;
  const write=await addLuaEntry(steamPath,appId,lua,overwrite);
  return {generation:{appId,appName:game?.name||`App ${appId}`,lua,depotCount:1,dlcCount:options.includeDlc?1:0,pinnedManifestCount:options.pinManifest?1:0,skippedKeyless:[],steps:['demo catalogue','generated Lua']},write,library:clone(demoLua)};
}
export async function remoteLibraryImport(steamPath:string,appId:number,overwrite=true):Promise<RemoteImportResult> {
  if(desktop)return invoke('remote_library_import_command',{steamPath,appId,overwrite});
  const write=await addLuaEntry(steamPath,appId,`addappid(${appId})\r\n`,overwrite);
  return {appId,imported:[write],skipped:[],archiveBytes:1024,luaFiles:1,attempts:1,steps:['demo remote package','imported Lua']};
}

function demoRelease(source:string):KernelReleaseArtifact {
  return {sourceUrl:source,releaseTag:'v1.5.4.1',version:'1.5.4.1',sourceCommit:'demo',assetName:'steamtools-next-kernel-1.5.4.1-windows-x64.zip',downloadUrl:`${source}/releases/download/v1.5.4.1/steamtools-next-kernel-1.5.4.1-windows-x64.zip`,archiveSha256:'0'.repeat(64),artifactSha256:'1'.repeat(64),artifactRoot:'C:\\Demo\\cache\\kernel-releases\\v1.5.4.1',cacheHit:false};
}
export async function installKernel(request:RemoteKernelInstallRequest):Promise<RemoteKernelInstallReceipt> {
  if(desktop)return invoke('install_remote_kernel_command',{request});
  const release=demoRelease(DEFAULT_KERNEL_RELEASE_SOURCE),plan=demoPlan(request,release);const next:{[K in keyof KernelInstallation]:KernelInstallation[K]}={...clone(demoKernelInstallation),status:'enabled',injectionMethod:'dwmapi',version:release.version,versionSource:'artifactManifest',artifactVersion:release.version,artifactSha256:release.artifactSha256,installedSha256:release.artifactSha256,hashMatch:true,runtimeLoadVerified:false,files:[]};
  const receipt=demoReceipt('install',plan,next);demoKernelInstallation=next;return {release,receipt};
}
export async function uninstallKernel(request:KernelUninstallRequest):Promise<KernelOperationReceipt> {
  if(desktop)return invoke('uninstall_kernel_command',{request});
  const plan={...demoPlan({...request,installExampleConfig:false},demoRelease(DEFAULT_KERNEL_RELEASE_SOURCE)),operation:'uninstall' as const,expectedStatus:'notInstalled',expectedInjectionMethod:'none'};
  const next={...clone(demoKernelInstallation),status:'notInstalled' as const,injectionMethod:'none' as const,version:null,files:[]};const receipt=demoReceipt('uninstall',plan,next);demoKernelInstallation=next;return receipt;
}

export async function pollManifest(steamPath:string,cursor:ManifestLogCursor,mode:'code'|'manifestHub'='code',apiKey:string|null=null,limit=10,download=true):Promise<ManifestPollResponse> {
  if(desktop)return invoke('poll_manifest_command',{steamPath,cursor,mode,apiKey,limit,download});
  return {cursor:{...clone(cursor),offset:cursor.offset+256},batch:{poll:{logPath:`${steamPath}\\logs\\content_log.txt`,reset:false,bytesRead:256,requests:[],malformedLines:0},downloaded:[],failed:[]}};
}

export async function loadSettings():Promise<Settings> {
  if(desktop)return migrateKernelSettings(await invoke<Settings>('load_settings_command'));
  try{return migrateKernelSettings({...defaults,...JSON.parse(localStorage.getItem('stn-demo-settings')||'{}')});}catch{return {...defaults};}
}
export async function saveSettings(settings:Settings):Promise<void> {
  const migrated=migrateKernelSettings(settings);if(desktop)await invoke('save_settings_command',{settings:migrated});else localStorage.setItem('stn-demo-settings',JSON.stringify(migrated));
}
export async function chooseDirectory():Promise<string|null> {
  if(!desktop)return null;const path=await open({directory:true,multiple:false});return typeof path==='string'?path:null;
}
export async function chooseFiles():Promise<string[]> {
  if(!desktop)return [];
  const paths=await open({directory:false,multiple:true,filters:[{name:'Steam Lua resources',extensions:['lua','bin','manifest','zip','tar','gz','tgz']}]});
  if(Array.isArray(paths))return paths;return typeof paths==='string'?[paths]:[];
}
export async function exportReport(snapshot:SteamSnapshot|null,kernelInstallation:KernelInstallation|null=null):Promise<string> {
  if(desktop)return invoke('export_report_command',{snapshot,kernel:null,comparison:null,installedKernel:null,kernelInstallation});
  const json=JSON.stringify({schemaVersion:1,demo:true,inspectionMode:'management',runtimeCompatibilityVerified:false,steam:snapshot,kernelInstallation},null,2);
  const url=URL.createObjectURL(new Blob([json],{type:'application/json'}));const a=document.createElement('a');a.href=url;a.download='steamtools-next-demo.json';a.click();setTimeout(()=>URL.revokeObjectURL(url),1000);return 'steamtools-next-demo.json';
}

const demoAchievementStates=new Map<string,boolean>();
export async function listAchievements(steamPath:string|null,appId:number,locale:string):Promise<AchievementCatalog> {
  if(desktop)return invoke('list_achievements_command',{steamPath,appId,locale});
  const chinese=locale.startsWith('zh'),japanese=locale==='ja';
  const labels=locale==='zh-TW'?[['第一步','完成教學關卡。'],['探索者','發現所有範例地點。'],['完美挑戰','完成最終挑戰。']]:chinese?[['第一步','完成教学关卡。'],['探索者','发现所有示例地点。'],['完美挑战','完成最终挑战。']]:japanese?[['最初の一歩','チュートリアルをクリアする。'],['探検家','すべてのサンプル地点を発見する。'],['完璧な挑戦','最後のチャレンジをクリアする。']]:[['First steps','Complete the tutorial.'],['Explorer','Discover every sample location.'],['Perfect challenge','Complete the final challenge.']];
  return {appId,requestedLanguage:locale,resolvedLanguage:chinese?locale:japanese?'ja':'en',source:'demo',connected:false,writable:true,error:null,
    achievements:labels.map(([displayName,description],index)=>({id:`SAMPLE_${index+1}`,displayName,description,icon:'',iconGray:'',unlocked:demoAchievementStates.get(`${appId}:${index}`)??index===0,unlockTime:null,hidden:false,protected:false}))};
}
export async function applyAchievementChanges(steamPath:string|null,appId:number,locale:string,changes:AchievementChange[]):Promise<AchievementApplyResult> {
  if(desktop)return invoke('apply_achievement_changes_command',{steamPath,appId,locale,changes});
  for(const change of changes)demoAchievementStates.set(`${appId}:${Number(change.id.replace('SAMPLE_',''))-1}`,change.unlock);
  return {appId,confirmed:true,results:changes.map(change=>({...change,success:true,error:null})),catalog:await listAchievements(steamPath,appId,locale)};
}

export async function loadGameCover(steamPath:string|null,appId:number,language:string,candidates:string[],cacheOnly=false):Promise<import('./types').GameCover|null>{return desktop?invoke('load_game_cover_command',{steamPath,appId,language,candidates,cacheOnly}):null}

export async function loadAchievementImage(appId:number,image:string,fallbackImage:string|null=null,cacheOnly=false):Promise<import('./types').GameCover|null>{return desktop?invoke('load_achievement_image_command',{appId,image,fallbackImage,cacheOnly}):null}

export async function getCurrentSteamSession(steamPath:string|null):Promise<import('./types').CurrentSteamSession>{
  if(desktop)return invoke('current_steam_session_command',{steamPath});
  return {connected:true,source:'demo',accountLabel:'Preview account',accountFingerprint:'demo-account',steamPath:'C:\\Demo\\Steam',games:demoSteam.apps.map(app=>({appId:app.appid,name:app.name,installed:true,subscribed:true})),capabilities:{subscriptionChecks:true,installedDepotKeys:true,freshDepotKeyRequests:false,localManifests:true,achievementSchema:true},warnings:[],error:null};
}
export async function extractCurrentSession(steamPath:string|null,appId:number,locale:string,expectedAccountFingerprint:string):Promise<import('./types').CurrentSessionExtraction>{
  if(desktop)return invoke('extract_current_session_command',{steamPath,appId,locale,expectedAccountFingerprint});
  const session=await getCurrentSteamSession(steamPath);
  return {session,appId,appName:session.games.find(game=>game.appId===appId)?.name||`AppID ${appId}`,subscribed:true,scope:'installedDepots',depots:[{depotId:appId+1,manifestId:null,keyAvailable:false,keySource:null,missingReason:'Preview data'}],keyedDepotCount:0,lua:`-- Preview only\naddappid(${appId})\n`,assets:[],complete:false,warnings:[]};
}
export async function exportCurrentSession(steamPath:string|null,appId:number,locale:string,expectedAccountFingerprint:string,destinationDirectory:string):Promise<import('./types').ExtractionExportReceipt>{
  if(desktop)return invoke('export_current_session_command',{steamPath,appId,locale,expectedAccountFingerprint,destinationDirectory});
  return {appId,destinationDirectory,files:[],keyedDepotCount:0,warnings:['Preview data']};
}
