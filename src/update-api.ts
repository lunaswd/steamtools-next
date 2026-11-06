import { invoke, isTauri } from '@tauri-apps/api/core';

export type UpdateStatus = 'available'|'current'|'notInstalled'|'error'|'unknown';
export interface UpdateError {messageKey?:string;detail?:string}
export interface UpdateCheck {
  status:UpdateStatus;
  currentVersion:string|null;
  latestVersion:string|null;
  releaseTag?:string|null;
  currentSha256?:string|null;
  latestSha256?:string|null;
  error?:UpdateError|null;
}
export interface UpdatesState {tool:UpdateCheck;kernel:UpdateCheck;checkedAt:number}
export interface ToolUpdateReceipt {prepared:boolean;restarting:boolean}

/** Fixed release repositories are owned by the native bridge, never by UI input. */
export async function checkUpdates(steamPath:string|null):Promise<UpdatesState> {
  if(isTauri())return invoke('check_updates_command',{steamPath});
  const unknown:UpdateCheck={status:'unknown',currentVersion:null,latestVersion:null};
  return {tool:{...unknown},kernel:{...unknown},checkedAt:Math.floor(Date.now()/1000)};
}

export async function installToolUpdate(expectedVersion:string):Promise<ToolUpdateReceipt> {
  if(!isTauri())throw {messageKey:'updates.desktopOnly',detail:''};
  return invoke('install_tool_update_command',{expectedVersion});
}
