import { t, getLocale } from './i18n';
import { checkUpdates, installToolUpdate } from './update-api';
import type { UpdateCheck, UpdateError, UpdatesState, ToolUpdateReceipt } from './update-api';

export interface UpdateControllerOptions {
  getSteamPath():string|null;
  getAppVersion():string;
  isAppBusy():boolean;
  onChange():void;
  onBeforeApply?():void;
  onApplyFailed?():void;
}
export interface UpdateControllerApi {
  check(path:string|null):Promise<UpdatesState>;
  install(expectedVersion:string):Promise<ToolUpdateReceipt>;
}
const defaultApi:UpdateControllerApi={check:checkUpdates,install:installToolUpdate};
const esc=(value:unknown)=>String(value??'').replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]!));
const tr=(key:string,args?:Record<string,string|number>)=>esc(t(key,args));
const updateIcon='<svg class="icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.65" aria-hidden="true"><path d="M20 7v5h-5M4 17v-5h5M6 7a7 7 0 0 1 11.8-2L20 8M4 16l2.2 3A7 7 0 0 0 18 17"/></svg>';

/** Release-only checks: no accounts, library contents or private paths leave this app. */
export class UpdateController {
  open=false;
  checking=false;
  confirmation=false;
  phase:'idle'|'preparing'|'restarting'='idle';
  result:UpdatesState|null=null;
  error:UpdateError|null=null;
  private checkRequest=0;
  private startupChecked=false;
  private suppressAutomaticPopup=false;
  private dismissed=new Set<string>();
  private ownerDocument:Document|null=null;
  private previouslyFocused:HTMLElement|null=null;
  constructor(private options:UpdateControllerOptions,private api:UpdateControllerApi=defaultApi) {}
  private changed(){this.options.onChange()}
  private fingerprint(check:UpdateCheck,kind:'tool'|'kernel'){return `${kind}:${check.releaseTag||''}:${check.latestVersion||''}:${check.latestSha256||''}`}
  private hasNewUpdate(){return !!this.result&&(['tool','kernel'] as const).some(kind=>this.result![kind].status==='available'&&!this.dismissed.has(this.fingerprint(this.result![kind],kind)))}
  private notice(error:unknown):UpdateError {
    if(error&&typeof error==='object'){
      const e=error as Record<string,unknown>;
      return {messageKey:typeof e.messageKey==='string'?e.messageKey:'updates.failed',detail:typeof e.detail==='string'?e.detail:error instanceof Error?error.message:''};
    }
    return {messageKey:'updates.failed',detail:typeof error==='string'?error:''};
  }
  get applying(){return this.phase!=='idle'}
  get toolAvailable(){return this.result?.tool.status==='available'&&!!this.result.tool.latestVersion}
  menuButton():string {
    return `<button id="updates-open" class="button secondary update-menu-button" data-update-action="open" ${this.applying?'disabled':''}>${updateIcon}<span>${tr('updates.check')}</span>${this.toolAvailable?'<span class="update-menu-dot" aria-hidden="true"></span>':''}</button>`;
  }
  kernelBanner():string {
    const kernel=this.result?.kernel;
    return kernel?.status==='available'?`<aside class="kernel-update-banner" role="status">${updateIcon}<div><strong>${tr('updates.kernelAvailable',{version:kernel.latestVersion||t('common.unknown')})}</strong><p>${tr('updates.kernelInstallHint')}</p></div></aside>`:'';
  }
  private checkedTime():string {
    if(!this.result?.checkedAt)return t('updates.notChecked');
    try{return new Intl.DateTimeFormat(getLocale(),{dateStyle:'short',timeStyle:'short'}).format(new Date(this.result.checkedAt*1000))}catch{return t('updates.notChecked')}
  }
  private row(kind:'tool'|'kernel'):string {
    const check=this.result?.[kind];
    const status=this.checking?'checking':check?.status||'unknown';
    const current=check?.currentVersion||(kind==='tool'?this.options.getAppVersion():null);
    const error=check?.error;
    return `<article class="update-item ${check?.status==='available'?'available':''}" data-update-kind="${kind}"><div class="update-item-heading"><h3>${tr(kind==='tool'?'updates.tool':'updates.kernel')}</h3><span class="update-status ${esc(status)}">${tr(`updates.status.${status}`)}</span></div><dl><div><dt>${tr('updates.current')}</dt><dd>${esc(current||'—')}</dd></div><div><dt>${tr('updates.latest')}</dt><dd>${esc(check?.latestVersion||'—')}</dd></div></dl>${kind==='kernel'&&check?.status==='available'?`<p class="update-item-hint">${tr('updates.kernelInstallHint')}</p>`:''}${error?this.errorMarkup(error):''}</article>`;
  }
  private errorMarkup(error:UpdateError):string {
    const key=error.messageKey||'updates.failed';
    const message=t(key)===key?t('updates.failed'):t(key);
    return `<div class="update-error" role="alert"><strong>${esc(message)}</strong>${error.detail?`<details><summary>${tr('diagnostics.technicalEvidence')}</summary><pre>${esc(error.detail)}</pre></details>`:''}</div>`;
  }
  markup():string {
    if(!this.open)return '';
    const blocked=this.applying||this.options.isAppBusy();
    const version=this.result?.tool.latestVersion||'';
    return `<div class="update-dialog-overlay" data-update-overlay><section id="update-dialog" class="update-dialog" role="dialog" aria-modal="true" aria-labelledby="update-dialog-title" tabindex="-1" aria-busy="${this.checking||this.applying}"><header><div>${updateIcon}<h2 id="update-dialog-title">${tr('updates.title')}</h2></div><button class="icon-button" id="updates-close" data-update-action="close" aria-label="${tr('common.close')}" ${this.applying?'disabled':''}>×</button></header><p class="update-privacy-note">${tr('updates.privacy')}</p><div class="update-items">${this.row('tool')}${this.row('kernel')}</div>${this.error?this.errorMarkup(this.error):''}${this.applying?`<div class="update-progress" role="status"><span class="spinner"></span><div><strong>${tr(this.phase==='preparing'?'updates.preparing':'updates.restarting')}</strong><p>${tr('updates.keepOpen')}</p></div></div>`:this.confirmation?`<div class="update-confirmation" role="alert"><strong>${tr('updates.confirmTitle',{version})}</strong><p>${tr('updates.confirmBody')}</p><div><button id="updates-confirm" class="button primary" data-update-action="confirm" ${blocked?'disabled':''}>${tr('updates.updateRestart')}</button><button class="button secondary" data-update-action="cancel" ${blocked?'disabled':''}>${tr('common.cancel')}</button></div></div>`:`<div class="update-dialog-actions"><button class="button secondary" id="updates-refresh" data-update-action="refresh" ${this.checking||blocked?'disabled':''}>${updateIcon}${tr('updates.check')}</button>${this.toolAvailable?`<button class="button primary" id="updates-install" data-update-action="install" ${this.checking||blocked?'disabled':''}>${tr('updates.updateRestart')}</button>`:''}<button class="button secondary" data-update-action="close" ${this.applying?'disabled':''}>${tr('updates.later')}</button></div>`}<footer>${tr('updates.lastChecked')}: ${esc(this.checkedTime())}</footer></section></div>`;
  }
  async check(automatic=false):Promise<void> {
    if(this.checking||this.applying)return;
    if(automatic&&this.startupChecked)return;
    if(automatic)this.startupChecked=true;
    const request=++this.checkRequest;
    this.checking=true;this.error=null;this.confirmation=false;this.changed();
    try {
      const result=await this.api.check(this.options.getSteamPath());
      if(request!==this.checkRequest)return;
      this.result=result;
      if(automatic&&!this.suppressAutomaticPopup&&this.hasNewUpdate())this.openDialog();
    } catch(error) {
      if(request!==this.checkRequest)return;
      this.error=this.notice(error);
    } finally {
      if(request===this.checkRequest){this.checking=false;this.changed()}
    }
  }
  openDialog(){
    if(!this.open&&typeof document!=='undefined')this.previouslyFocused=document.activeElement instanceof HTMLElement?document.activeElement:null;
    this.open=true;this.confirmation=false;this.changed();
  }
  async openAndCheck(){this.openDialog();await this.check(false)}
  close(){
    if(this.applying)return;
    this.suppressAutomaticPopup=true;
    if(this.result)for(const kind of ['tool','kernel'] as const)if(this.result[kind].status==='available')this.dismissed.add(this.fingerprint(this.result[kind],kind));
    this.open=false;this.confirmation=false;this.changed();
    const previous=this.previouslyFocused;
    const target=previous?.isConnected?previous:previous?.id?this.ownerDocument?.getElementById(previous.id):null;
    target?.focus({preventScroll:true});
  }
  askToInstall(){if(this.toolAvailable&&!this.checking&&!this.applying&&!this.options.isAppBusy()){this.confirmation=true;this.error=null;this.changed()}}
  async confirmInstall():Promise<void> {
    if(!this.confirmation||!this.toolAvailable||this.checking||this.applying||this.options.isAppBusy())return;
    const expectedVersion=this.result!.tool.latestVersion!;
    this.confirmation=false;this.phase='preparing';this.error=null;
    try {
      this.options.onBeforeApply?.();this.changed();
      const receipt=await this.api.install(expectedVersion);
      if(!receipt.prepared||!receipt.restarting)throw {messageKey:'updates.failed',detail:''};
      this.phase='restarting';this.changed();
      // Only native code may exit/restart. A browser reload would not apply files.
    } catch(error) {
      this.phase='idle';this.error=this.notice(error);this.options.onApplyFailed?.();this.changed();
    }
  }
  kernelInstalled(version:string|null,sha:string|null){
    const kernel=this.result?.kernel;if(!kernel)return;
    kernel.currentVersion=version;kernel.currentSha256=sha;
    if(!version&&!sha)kernel.status='notInstalled';
    else if(kernel.latestSha256?sha===kernel.latestSha256:version===kernel.latestVersion)kernel.status='current';
    this.changed();
  }
  bind(root:HTMLElement){
    const owner=root.ownerDocument;
    if(this.ownerDocument!==owner){this.dispose();this.ownerDocument=owner;owner.addEventListener('keydown',this.onKeyDown)}
    root.querySelectorAll<HTMLElement>('[data-update-action]').forEach(element=>element.addEventListener('click',()=>{
      switch(element.dataset.updateAction){
        case'open':void this.openAndCheck();break;
        case'close':this.close();break;
        case'refresh':void this.check(false);break;
        case'install':this.askToInstall();break;
        case'confirm':void this.confirmInstall();break;
        case'cancel':this.confirmation=false;this.changed();break;
      }
    }));
    root.querySelector<HTMLElement>('[data-update-overlay]')?.addEventListener('click',event=>{if(event.target===event.currentTarget)this.close()});
    if(this.open){const dialog=root.querySelector<HTMLElement>('#update-dialog');if(dialog&&!dialog.contains(owner.activeElement))dialog.focus({preventScroll:true})}
  }
  private onKeyDown=(event:KeyboardEvent)=>{
    if(!this.open)return;
    if(event.key==='Escape'){event.preventDefault();this.close();return}
    if(event.key!=='Tab')return;
    const dialog=this.ownerDocument?.getElementById('update-dialog');if(!dialog)return;
    const controls=Array.from(dialog.querySelectorAll<HTMLElement>('button:not([disabled]),summary,a[href],[tabindex="0"]'));
    if(!controls.length){event.preventDefault();dialog.focus();return}
    const active=this.ownerDocument!.activeElement,index=controls.indexOf(active as HTMLElement);
    if(event.shiftKey&&(index<=0)){event.preventDefault();controls.at(-1)!.focus()}
    else if(!event.shiftKey&&(index<0||index===controls.length-1)){event.preventDefault();controls[0].focus()}
  };
  dispose(){this.ownerDocument?.removeEventListener('keydown',this.onKeyDown);this.ownerDocument=null}
}
