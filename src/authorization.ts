import { t } from './i18n';
import {
  authorizationSession, chooseAuthorizationDestination, chooseAuthorizationSource,
  extractAuthorizationTicket, importAuthorizationTicket, inspectAuthorizationTicket,
} from './authorization-api';
import type {
  AuthorizationSessionSummary, AuthorizationTicketSummary, AuthorizationExportReceipt,
  AuthorizationImportReceipt, AuthorizationExtractRequest, AuthorizationImportRequest,
} from './authorization-api';

type Notice={messageKey:string;detail:string};
type Action='extract'|'import';
type Binding={action:Action;steamPath:string;appId:number;steamId:string;destination:string|null;sourcePath:string|null;sha256:string|null};
export interface AuthorizationControllerOptions {
  getSteamPath():string|null;
  isSteamRunning():boolean|undefined;
  isAppBusy():boolean;
  onChange():void;
  onBeforeAction?():void;
  onAfterAction?():void;
  onNotice?(message:string):void;
}
export interface AuthorizationControllerApi {
  session(path:string,appId:number):Promise<AuthorizationSessionSummary>;
  inspect(path:string):Promise<AuthorizationTicketSummary>;
  extract(request:AuthorizationExtractRequest):Promise<AuthorizationExportReceipt>;
  import(request:AuthorizationImportRequest):Promise<AuthorizationImportReceipt>;
  chooseSource():Promise<string|null>;
  chooseDestination():Promise<string|null>;
}
const defaultApi:AuthorizationControllerApi={session:authorizationSession,inspect:inspectAuthorizationTicket,
  extract:extractAuthorizationTicket,import:importAuthorizationTicket,chooseSource:chooseAuthorizationSource,
  chooseDestination:chooseAuthorizationDestination};
const esc=(value:unknown)=>String(value??'').replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]!));
const tr=(key:string,args?:Record<string,string|number>)=>esc(t(key,args));
const samePath=(a:string,b:string)=>a.replace(/^\\\\\?\\/,'').replace(/\//g,'\\').replace(/\\+$/,'').toLocaleLowerCase('en')===b.replace(/^\\\\\?\\/,'').replace(/\//g,'\\').replace(/\\+$/,'').toLocaleLowerCase('en');
const validId=(value:string)=>/^\d+$/.test(value)&&Number(value)>0&&Number(value)<=4294967295;
const validSummary=(s:AuthorizationTicketSummary)=>Number.isInteger(s.appId)&&s.appId>0&&s.appId<=4294967295&&/^\d+$/.test(s.steamId)&&s.steamId!=='0'&&/^[a-f0-9]{64}$/.test(s.sha256)&&Number.isInteger(s.appTicketBytes)&&s.appTicketBytes>=20&&s.appTicketBytes<=1024*1024&&Number.isInteger(s.eTicketBytes)&&s.eTicketBytes>0&&s.eTicketBytes<=1024*1024&&Number.isInteger(s.appIdOffset)&&s.appIdOffset>=0&&s.appIdOffset+4<=s.appTicketBytes&&Number.isInteger(s.steamIdOffset)&&s.steamIdOffset>=0&&s.steamIdOffset+8<=s.appTicketBytes&&s.validationScope==='fileStructureAndDeclaredIdentity';
const summaryMetadata=(s:AuthorizationTicketSummary):AuthorizationTicketSummary=>({appId:s.appId,steamId:s.steamId,appTicketBytes:s.appTicketBytes,eTicketBytes:s.eTicketBytes,sha256:s.sha256,sourcePath:s.sourcePath,appIdOffset:s.appIdOffset,steamIdOffset:s.steamIdOffset,validationScope:s.validationScope});

/** Metadata-only UI. The native worker, never the UI, verifies live ownership. */
export class AuthorizationController {
  appId='';
  session:AuthorizationSessionSummary|null=null;
  preview:AuthorizationTicketSummary|null=null;
  receipt:AuthorizationExportReceipt|AuthorizationImportReceipt|null=null;
  resultAction:Action|null=null;
  destination:string|null=null;
  error:Notice|null=null;
  busy=false;
  checking=false;
  confirmation:Binding|null=null;
  overwrite=false;
  private request=0;
  private loadedPath:string|null|undefined=undefined;
  constructor(private options:AuthorizationControllerOptions,private api:AuthorizationControllerApi=defaultApi){}
  private changed(){this.options.onChange()}
  private resetForPath(){
    const path=this.options.getSteamPath();
    if(this.loadedPath!==undefined&&path!==this.loadedPath)this.invalidate();
    this.loadedPath=path;
  }
  invalidate(){++this.request;this.session=null;this.receipt=null;this.resultAction=null;this.confirmation=null;this.checking=false;this.error=null;}
  private notice(error:unknown):Notice {
    const e=error&&typeof error==='object'?error as Record<string,unknown>:{};
    const codes:Record<string,string>={steam_session_offline:'error.steamSessionOffline',steam_session_signed_out:'error.steamSessionSignedOut',steam_session_identity_changed:'error.steamSessionChanged',steam_session_startup_timeout:'error.steamSessionStarting',steam_client_invalid:'error.steamClientInvalid'};
    const code=typeof e.code==='string'?e.code:'';
    const key=codes[code]||(typeof e.messageKey==='string'?e.messageKey:'');
    const known=(key.startsWith('authorization.')||key.startsWith('error.'))&&t(key)!==key;
    // Native errors can contain paths, but never raw authorization bytes. Keep
    // full raw exception values out of the UI even for malformed inputs.
    return {messageKey:known?key:'authorization.failed',detail:''};
  }
  private active():boolean {
    this.resetForPath();
    if(this.options.isSteamRunning()===false){this.invalidate();this.error={messageKey:'error.steamSessionOffline',detail:''};return false;}
    if(!this.options.getSteamPath()){this.invalidate();this.error={messageKey:'authorization.chooseSteam',detail:''};return false;}
    return true;
  }
  private usable(){return !!this.session?.connected&&this.session.subscribed&&this.session.appId===Number(this.appId)}
  setAppId(value:string){
    if(this.busy)return;
    this.appId=value;this.invalidate();this.changed();
  }
  private summary(s:AuthorizationTicketSummary):string {
    return `<dl class="authorization-summary"><div><dt>AppID</dt><dd>${s.appId}</dd></div><div><dt>${tr('authorization.owner')}</dt><dd>${esc(s.steamId)}</dd></div><div><dt>AppTicket</dt><dd>${s.appTicketBytes} ${tr('authorization.bytes')}</dd></div><div><dt>ETicket</dt><dd>${s.eTicketBytes} ${tr('authorization.bytes')}</dd></div><div class="authorization-hash"><dt>SHA-256</dt><dd><code>${esc(s.sha256)}</code></dd></div></dl>`;
  }
  markup():string {
    this.resetForPath();
    const stopped=this.options.isSteamRunning()===false;
    const blocked=this.busy||this.checking||this.options.isAppBusy();
    const usable=this.usable()&&!stopped;
    const status=this.checking?'authorization.checking':this.session?.connected?(this.session.subscribed?'authorization.owned':'authorization.notOwned'):'authorization.notVerified';
    const identityMismatch=!!this.preview&&!!this.session?.connected&&(this.preview.appId!==Number(this.appId)||this.preview.steamId!==this.session.steamId);
    return `<div class="authorization-page"><section class="surface authorization-session"><div class="section-heading"><h2>${tr('authorization.session')}</h2><span class="badge">${tr(status)}</span></div><p class="authorization-scope">${tr('authorization.scope')}</p>${stopped?`<div class="inline-notice warning" role="status">${tr('error.steamSessionOffline')}</div>`:''}<div class="authorization-app-controls"><label for="authorization-appid">AppID<input id="authorization-appid" inputmode="numeric" autocomplete="off" value="${esc(this.appId)}" placeholder="${tr('authorization.appIdPlaceholder')}" ${blocked?'disabled':''}></label><button id="authorization-verify" class="button secondary" data-authorization-action="verify" ${blocked||stopped||!validId(this.appId)?'disabled':''}>${tr('authorization.verify')}</button></div>${this.session?.connected?`<div class="authorization-account"><span>${tr('authorization.owner')}</span><code>${esc(this.session.steamId)}</code></div>`:''}${this.error?`<div class="authorization-error" role="alert">${tr(this.error.messageKey)}</div>`:''}</section><div class="authorization-columns"><section class="surface authorization-extract"><h2>${tr('authorization.extract')}</h2><p>${tr('authorization.extractDescription')}</p><button id="authorization-extract" class="button primary" data-authorization-action="extract" ${blocked||!usable?'disabled':''}>${tr('authorization.extract')}</button>${this.destination?`<p class="authorization-path"><code>${esc(this.destination)}</code></p>`:''}</section><section class="surface authorization-import"><h2>${tr('authorization.import')}</h2><p>${tr('authorization.importDescription')}</p><button id="authorization-source" class="button secondary" data-authorization-action="inspect" ${blocked?'disabled':''}>${tr('authorization.chooseFile')}</button>${identityMismatch?`<p class="authorization-error" role="alert">${tr('authorization.identityMismatch')}</p>`:''}${this.preview?`<p class="authorization-path"><code>${esc(this.preview.sourcePath||'')}</code></p>${this.summary(this.preview)}<p class="authorization-structure-note">${tr('authorization.structureOnly')}</p><button id="authorization-import" class="button primary" data-authorization-action="import" ${blocked||!usable||this.preview.appId!==Number(this.appId)||this.preview.steamId!==this.session?.steamId?'disabled':''}>${tr('authorization.import')}</button>`:''}</section></div>${this.confirmation?`<section class="surface authorization-confirmation" role="alert"><h3>${tr(this.confirmation.action==='extract'?'authorization.confirmExtract':'authorization.confirmImport')}</h3><dl><div><dt>AppID</dt><dd>${this.confirmation.appId}</dd></div><div><dt>${tr('authorization.owner')}</dt><dd>${esc(this.confirmation.steamId)}</dd></div></dl><code>${esc(this.confirmation.destination||this.confirmation.sourcePath||'')}</code><p>${tr('authorization.privateFile')}</p><label class="authorization-overwrite"><input id="authorization-overwrite" type="checkbox" ${this.overwrite?'checked':''} ${this.busy?'disabled':''}>${tr('authorization.overwrite')}</label><div><button class="button secondary" data-authorization-action="cancel" ${this.busy?'disabled':''}>${tr('common.cancel')}</button><button id="authorization-confirm" class="button primary" data-authorization-action="confirm" ${blocked?'disabled':''}>${tr(this.busy?'authorization.working':'authorization.confirm')}</button></div></section>`:''}${this.receipt?`<section class="surface authorization-result" role="status"><h3>${tr(this.resultAction==='extract'?'authorization.exported':'authorization.imported')}</h3>${this.summary(this.receipt.summary)}<code>${esc('outputPath'in this.receipt?this.receipt.outputPath:this.receipt.registryPath)}</code><p>${tr('authorization.runtimeNotVerified')}</p></section>`:''}<p class="authorization-privacy">${tr('authorization.privacy')}</p></div>`;
  }
  async verify(){
    if(this.busy||this.checking||this.options.isAppBusy())return;
    if(!this.active()||!validId(this.appId)){this.changed();return;}
    const path=this.options.getSteamPath()!,appId=Number(this.appId),request=++this.request;
    this.checking=true;this.error=null;this.session=null;this.receipt=null;this.confirmation=null;this.changed();
    try{
      const session=await this.api.session(path,appId);
      if(request!==this.request||path!==this.options.getSteamPath()||appId!==Number(this.appId))return;
      if(!this.active())return;
      if(session.appId!==appId||!samePath(session.steamRoot,path)||!session.connected||!/^\d+$/.test(session.steamId)||session.steamId==='0')throw {messageKey:'authorization.sessionUnavailable'};
      this.session={steamRoot:session.steamRoot,appId:session.appId,steamId:session.steamId,connected:session.connected,subscribed:session.subscribed};
      if(!session.subscribed)this.error={messageKey:'authorization.notOwned',detail:''};
    }catch(error){if(request===this.request){this.session=null;this.error=this.notice(error)}}
    finally{if(request===this.request)this.checking=false;this.changed();}
  }
  async inspect(){
    if(this.busy||this.checking||this.options.isAppBusy())return;
    const request=++this.request;this.preview=null;this.confirmation=null;this.receipt=null;this.error=null;this.checking=true;this.changed();
    try{
      const path=await this.api.chooseSource();if(!path||request!==this.request)return;
      const summary=await this.api.inspect(path);
      if(request!==this.request)return;
      if(!validSummary(summary)||!summary.sourcePath||!samePath(summary.sourcePath,path))throw {messageKey:'authorization.invalidFile'};
      this.preview=summaryMetadata(summary);
      if(Number(this.appId)!==summary.appId){this.appId=String(summary.appId);this.session=null;}
    }catch(error){if(request===this.request)this.error=this.notice(error)}
    finally{if(request===this.request)this.checking=false;this.changed();}
  }
  async ask(action:Action){
    if(this.busy||this.checking||this.options.isAppBusy()||!this.active()||!this.usable()){this.changed();return;}
    const path=this.options.getSteamPath()!,session=this.session!,appId=Number(this.appId),request=this.request;
    let destination:string|null=null;
    if(action==='extract'){
      this.checking=true;this.changed();
      try{destination=await this.api.chooseDestination()}catch(error){this.error=this.notice(error);this.changed();return;}
      finally{if(request===this.request)this.checking=false;this.changed();}
      if(!destination)return;
    }
    if(!this.active()||request!==this.request||path!==this.options.getSteamPath()||this.session!==session||appId!==Number(this.appId)){this.error={messageKey:'authorization.contextChanged',detail:''};this.changed();return;}
    if(action==='import'&&(!this.preview||this.preview.appId!==appId||this.preview.steamId!==session.steamId||!this.preview.sourcePath)){this.error={messageKey:'authorization.contextChanged',detail:''};this.changed();return;}
    this.confirmation={action,steamPath:path,appId,steamId:session.steamId,destination,sourcePath:this.preview?.sourcePath||null,sha256:this.preview?.sha256||null};
    this.destination=destination;this.overwrite=false;this.receipt=null;this.error=null;this.changed();
  }
  async confirm(){
    if(this.busy||this.checking||this.options.isAppBusy())return;
    const binding=this.confirmation;
    if(!binding||!this.active()) {this.changed();return;}
    if(binding.steamPath!==this.options.getSteamPath()||binding.appId!==Number(this.appId)||binding.steamId!==this.session?.steamId||!this.usable()||binding.action==='import'&&(binding.sourcePath!==this.preview?.sourcePath||binding.sha256!==this.preview?.sha256)){this.confirmation=null;this.error={messageKey:'authorization.contextChanged',detail:''};this.changed();return;}
    const overwrite=this.overwrite,request=this.request;
    this.busy=true;this.error=null;this.receipt=null;this.changed();
    try{
      this.options.onBeforeAction?.();
      const fresh=await this.api.session(binding.steamPath,binding.appId);
      if(request!==this.request||!this.active()||this.options.getSteamPath()!==binding.steamPath||!fresh.connected||!fresh.subscribed||fresh.appId!==binding.appId||fresh.steamId!==binding.steamId||!samePath(fresh.steamRoot,binding.steamPath))throw {messageKey:'authorization.contextChanged'};
      const receipt=binding.action==='extract'
        ?await this.api.extract({steamPath:binding.steamPath,appId:binding.appId,expectedSteamId:binding.steamId,destination:binding.destination!,overwrite})
        :await this.api.import({steamPath:binding.steamPath,appId:binding.appId,expectedSteamId:binding.steamId,sourcePath:binding.sourcePath!,expectedSha256:binding.sha256!,overwrite});
      if(request!==this.request||!this.active()||this.options.getSteamPath()!==binding.steamPath||!validSummary(receipt.summary)||receipt.summary.appId!==binding.appId||receipt.summary.steamId!==binding.steamId||receipt.runtimeAuthorizationVerified!==false||(!overwrite&&receipt.replaced))throw {messageKey:'authorization.contextChanged'};
      if(binding.action==='extract'&&(!('outputPath'in receipt)||!samePath(receipt.outputPath,binding.destination!)))throw {messageKey:'authorization.failed'};
      if(binding.action==='import'&&(!('writeVerified'in receipt)||receipt.writeVerified!==true||receipt.summary.sha256!==binding.sha256))throw {messageKey:'authorization.failed'};
      this.receipt='outputPath'in receipt
        ?{summary:summaryMetadata(receipt.summary),outputPath:receipt.outputPath,replaced:receipt.replaced,runtimeAuthorizationVerified:false}
        :{summary:summaryMetadata(receipt.summary),registryPath:receipt.registryPath,replaced:receipt.replaced,writeVerified:true,runtimeAuthorizationVerified:false};
      this.resultAction=binding.action;this.confirmation=null;
      this.options.onNotice?.(t(binding.action==='extract'?'authorization.exported':'authorization.imported'));
    }catch(error){this.error=this.notice(error);if(this.error.messageKey.startsWith('error.steam'))this.session=null;}
    finally{this.busy=false;this.options.onAfterAction?.();this.changed();}
  }
  bind(root:HTMLElement){
    root.querySelector<HTMLInputElement>('#authorization-appid')?.addEventListener('input',event=>{this.setAppId((event.target as HTMLInputElement).value)});
    root.querySelector<HTMLInputElement>('#authorization-appid')?.addEventListener('change',()=>this.changed());
    root.querySelector<HTMLInputElement>('#authorization-overwrite')?.addEventListener('change',event=>{this.overwrite=(event.target as HTMLInputElement).checked});
    root.querySelectorAll<HTMLElement>('[data-authorization-action]').forEach(button=>button.addEventListener('click',()=>{
      const action=button.dataset.authorizationAction;
      if(action==='verify')void this.verify();else if(action==='inspect')void this.inspect();
      else if(action==='extract'||action==='import')void this.ask(action);else if(action==='confirm')void this.confirm();
      else if(action==='cancel'&&!this.busy){this.confirmation=null;this.changed();}
    }));
  }
  dispose(){++this.request;}
}
