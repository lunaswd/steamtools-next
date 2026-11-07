import { invoke, isTauri } from '@tauri-apps/api/core';
import type { SteamAccountState, SteamLifecycleReceipt } from './types';

export async function steamAccountState(steamPath:string|null):Promise<SteamAccountState> {
  if(isTauri())return invoke('steam_account_state_command',{steamPath});
  return {steamRoot:steamPath||'C:\\Demo\\Steam',running:false,currentSteamId:null,currentAccountName:null,
    accounts:[],identitySource:'demo',warnings:[]};
}

async function lifecycle(command:string):Promise<SteamLifecycleReceipt> {
  // Browser previews do not emulate a successful restart or a signed-in session.
  throw {code:'preview_only',messageKey:'steamAccounts.previewOnly',detail:`${command} requires the desktop application`};
}

export async function restartSteam(steamPath:string|null):Promise<SteamLifecycleReceipt> {
  return isTauri()?invoke('restart_steam_command',{steamPath}):lifecycle('restart');
}
export async function reloginSteam(steamPath:string|null):Promise<SteamLifecycleReceipt> {
  return isTauri()?invoke('relogin_steam_command',{steamPath}):lifecycle('relogin');
}

/** Select only a Steam-owned saved account; never accept authentication data. */
export async function switchSavedSteamAccount(steamPath:string|null,steamId:string):Promise<SteamLifecycleReceipt> {
  return isTauri()?invoke('switch_saved_steam_account_command',{steamPath,steamId}):lifecycle('switchAccount');
}
