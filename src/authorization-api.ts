import { invoke, isTauri } from '@tauri-apps/api/core';
import { open, save } from '@tauri-apps/plugin-dialog';

/** Native/private ticket bytes never cross this UI boundary. */
export interface AuthorizationSessionSummary {
  steamRoot:string; appId:number; steamId:string; connected:boolean; subscribed:boolean;
}
export interface AuthorizationTicketSummary {
  appId:number; steamId:string; appTicketBytes:number; eTicketBytes:number; sha256:string;
  sourcePath:string|null; appIdOffset:number; steamIdOffset:number;
  validationScope:'fileStructureAndDeclaredIdentity';
}
export interface AuthorizationExportReceipt {
  summary:AuthorizationTicketSummary; outputPath:string; replaced:boolean;
  runtimeAuthorizationVerified:false;
}
export interface AuthorizationImportReceipt {
  summary:AuthorizationTicketSummary; registryPath:string; replaced:boolean;
  writeVerified:true; runtimeAuthorizationVerified:false;
}
export interface AuthorizationExtractRequest {
  steamPath:string; appId:number; expectedSteamId:string; destination:string; overwrite:boolean;
}
export interface AuthorizationImportRequest {
  steamPath:string; appId:number; expectedSteamId:string; sourcePath:string;
  expectedSha256:string; overwrite:boolean;
}
function desktopOnly(){
  if(!isTauri())throw {code:'preview_only',messageKey:'authorization.desktopOnly',detail:''};
}
export async function authorizationSession(steamPath:string,appId:number):Promise<AuthorizationSessionSummary> {
  desktopOnly();return invoke('authorization_session_command',{steamPath,appId});
}
export async function inspectAuthorizationTicket(sourcePath:string):Promise<AuthorizationTicketSummary> {
  desktopOnly();return invoke('inspect_authorization_ticket_command',{sourcePath});
}
export async function extractAuthorizationTicket(request:AuthorizationExtractRequest):Promise<AuthorizationExportReceipt> {
  desktopOnly();return invoke('extract_authorization_ticket_command',{...request});
}
export async function importAuthorizationTicket(request:AuthorizationImportRequest):Promise<AuthorizationImportReceipt> {
  desktopOnly();return invoke('import_authorization_ticket_command',{...request});
}
export async function chooseAuthorizationSource():Promise<string|null> {
  desktopOnly();const selected=await open({multiple:false,directory:false,filters:[{name:'Steam ticket text',extensions:['txt']}]});
  return typeof selected==='string'?selected:null;
}
export async function chooseAuthorizationDestination():Promise<string|null> {
  desktopOnly();const selected=await save({defaultPath:'tickets.txt',filters:[{name:'Steam ticket text',extensions:['txt']}]});
  return typeof selected==='string'?selected:null;
}
