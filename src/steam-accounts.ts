import { t } from './i18n';
import { reloginSteam, restartSteam, steamAccountState, switchSavedSteamAccount } from './steam-account-api';
import type { SteamAccountState, SteamLifecycleReceipt, SteamSavedAccount } from './types';

export type SteamAccountAction = 'restart'|'relogin'|'switchAccount';
type ErrorNotice = {messageKey:string;detail:string};
export interface SteamAccountMenuOptions {
  getSteamPath():string|null;
  isAppBusy():boolean;
  onChange():void;
  onBeforeAction?(action:SteamAccountAction):void;
  onAfterAction?():void;
  onCompleted?(receipt:SteamLifecycleReceipt):Promise<void>|void;
  onNotice?(message:string):void;
}
export interface SteamAccountMenuApi {
  state(path:string|null):Promise<SteamAccountState>;
  restart(path:string|null):Promise<SteamLifecycleReceipt>;
  relogin(path:string|null):Promise<SteamLifecycleReceipt>;
  switchAccount(path:string|null,steamId:string):Promise<SteamLifecycleReceipt>;
}
const defaultApi:SteamAccountMenuApi={state:steamAccountState,restart:restartSteam,relogin:reloginSteam,switchAccount:switchSavedSteamAccount};
const esc=(value:unknown)=>String(value??'').replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]!));
const tr=(key:string,args?:Record<string,string|number>)=>esc(t(key,args));
const sameSteamPath=(left:string,right:string)=>left.replace(/^\\\\\?\\/,'').replace(/\//g,'\\').replace(/\\+$/,'').toLocaleLowerCase('en')===right.replace(/^\\\\\?\\/,'').replace(/\//g,'\\').replace(/\\+$/,'').toLocaleLowerCase('en');
const accountIcon='<svg class="icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.65" aria-hidden="true"><circle cx="12" cy="8" r="4"/><path d="M4 21v-2a8 8 0 0 1 16 0v2"/></svg>';
const restartIcon='<svg class="icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.65" aria-hidden="true"><path d="M20 6v6h-6M20 12a8 8 0 1 0-2 6"/></svg>';
const loginIcon='<svg class="icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.65" aria-hidden="true"><path d="M13 4h7v16h-7M3 12h12m-4-4 4 4-4 4"/></svg>';

/** Lazy non-secret account reader and explicit native-Steam lifecycle controls. */
export class SteamAccountMenu {
  open=false;
  loading=false;
  busy=false;
  data:SteamAccountState|null=null;
  confirmation:SteamAccountAction|null=null;
  error:ErrorNotice|null=null;
  receipt:SteamLifecycleReceipt|null=null;
  private request=0;
  private confirmationPath:string|null=null;
  private confirmationAccount:SteamSavedAccount|null=null;
  private loadedPath:string|null|undefined=undefined;
  private ownerDocument:Document|null=null;
  constructor(private options:SteamAccountMenuOptions,private api:SteamAccountMenuApi=defaultApi) {}

  private changed(){this.options.onChange();}
  private errorNotice(error:unknown):ErrorNotice {
    const e=error&&typeof error==='object'?error as Record<string,unknown>:{};
    const key=typeof e.messageKey==='string'?e.messageKey:'';
    const translated=t(key);
    const known=(key.startsWith('steamAccounts.')||key.startsWith('error.'))&&translated!==key&&!/\{[A-Za-z0-9_]+\}/.test(translated);
    return {messageKey:known?key:'steamAccounts.operationFailed',
      detail:typeof e.detail==='string'?e.detail:error instanceof Error?error.message:''};
  }
  private resetForPath(){
    const path=this.options.getSteamPath();
    if(this.loadedPath!==undefined&&this.loadedPath!==path){++this.request;this.data=null;this.loading=false;this.error=null;this.receipt=null;this.confirmation=null;this.confirmationAccount=null;}
    this.loadedPath=path;
  }
  markup():string {
    this.resetForPath();
    const state=this.data;
    const subtitle=state?.currentAccountName||(state?t(state.running?'steamAccounts.running':'steamAccounts.stopped'):t('steamAccounts.openToRead'));
    const blocked=this.busy||this.options.isAppBusy();
    return `<div class="steam-account-controls"><button id="steam-account-button" class="steam-account-button" data-steam-account-action="toggle" aria-haspopup="dialog" aria-expanded="${this.open}" aria-controls="steam-account-menu" ${blocked?'disabled':''}>${accountIcon}<span><strong>${tr('steamAccounts.title')}</strong><small>${esc(subtitle)}</small></span><span class="steam-account-chevron" aria-hidden="true">${this.open?'⌄':'⌃'}</span></button>${this.open?this.panelMarkup():''}</div>`;
  }
  private panelMarkup():string {
    const data=this.data;
    const blocked=this.busy||this.options.isAppBusy()||this.loading;
    const available=!!(data?.steamRoot||this.options.getSteamPath());
    const selected=data?.accounts.find(account=>account.steamId===data.currentSteamId);
    const name=selected?.personaName||data?.currentAccountName;
    const status=this.loading?'steamAccounts.loading':this.busy?'steamAccounts.working':data?(data.running?'steamAccounts.running':'steamAccounts.stopped'):'steamAccounts.openToRead';
    const accounts=data?.accounts||[];
    return `<section id="steam-account-menu" class="steam-account-menu" role="dialog" aria-label="${tr('steamAccounts.title')}" tabindex="-1" aria-busy="${this.loading||this.busy}"><header><div><h2>${tr('steamAccounts.title')}</h2><span class="steam-account-status">${tr(status)}</span></div><button id="steam-account-close" class="icon-button" data-steam-account-action="close" aria-label="${tr('common.close')}">×</button></header><div class="steam-account-current"><span>${tr('steamAccounts.selectedAccount')}</span><strong>${esc(name||t('steamAccounts.noSelectedAccount'))}</strong>${selected?.accountName&&selected.accountName!==name?`<small>${esc(selected.accountName)}</small>`:''}</div><p class="steam-account-hint">${tr('steamAccounts.localAccountHint')}</p>${this.error?`<div class="steam-account-error" role="alert"><strong>${tr(this.error.messageKey)}</strong>${this.error.detail?`<details><summary>${tr('diagnostics.technicalEvidence')}</summary><pre>${esc(this.error.detail)}</pre></details>`:''}</div>`:''}${data?.warnings.length?`<ul class="steam-account-warnings">${data.warnings.map(w=>`<li>${esc(w)}</li>`).join('')}</ul>`:''}${this.receipt?`<div class="steam-account-result" role="status">${tr(this.receipt.operation==='switchAccount'?'steamAccounts.switchRequested':this.receipt.requiresNativeLogin?'steamAccounts.nativeLoginRequested':'steamAccounts.restartRequested')}${this.receipt.warnings.length?`<ul>${this.receipt.warnings.map(w=>`<li>${esc(w)}</li>`).join('')}</ul>`:''}</div>`:''}${this.confirmation?this.confirmationMarkup(blocked):`<div class="steam-account-actions"><button id="steam-account-relogin" class="button secondary" data-steam-account-action="relogin" ${blocked||!available?'disabled':''}>${loginIcon}${tr('steamAccounts.relogin')}</button><button id="steam-account-restart" class="button secondary" data-steam-account-action="restart" ${blocked||!available?'disabled':''}>${restartIcon}${tr('steamAccounts.restart')}</button></div>`}<div class="steam-account-list-heading"><span>${tr('steamAccounts.savedAccounts')}</span><button id="steam-account-refresh" class="steam-account-refresh" data-steam-account-action="refresh" ${blocked?'disabled':''}>${tr('common.refresh')}</button></div>${this.loading?`<div class="steam-account-empty" role="status">${tr('steamAccounts.loading')}</div>`:accounts.length?`<ul class="steam-saved-account-list">${accounts.map(account=>`<li><span class="steam-saved-account-avatar" aria-hidden="true">${esc((account.personaName||account.accountName||'?').slice(0,1))}</span><div><strong>${esc(account.personaName||account.accountName)}</strong><small>${esc(account.accountName)}</small></div>${this.savedAccountAction(account,blocked)}</li>`).join('')}</ul>`:`<p class="steam-account-empty">${tr('steamAccounts.noSavedAccounts')}</p>`}<p class="steam-account-login-note">${tr('steamAccounts.nativeLoginHint')}</p></section>`;
  }
  private savedAccountAction(account:SteamSavedAccount,blocked:boolean):string {
    const current=account.isCurrent||account.steamId===this.data?.currentSteamId;
    return `<button type="button" class="steam-account-switch ${current?'steam-account-selected':''}" data-steam-account-action="switchAccount" data-steam-account-id="${esc(account.steamId)}" aria-label="${current?tr('steamAccounts.selected'):tr('steamAccounts.switchTo',{name:account.personaName||account.accountName})}" ${blocked||current?'disabled':''}>${tr(current?'steamAccounts.selected':'steamAccounts.switch')}</button>`;
  }
  private confirmationMarkup(blocked:boolean):string {
    const action=this.confirmation!;
    const target=this.confirmationAccount;
    const title=action==='switchAccount'?'steamAccounts.confirmSwitchTitle':action==='relogin'?'steamAccounts.confirmReloginTitle':'steamAccounts.confirmRestartTitle';
    const body=action==='switchAccount'?'steamAccounts.confirmSwitch':action==='relogin'?'steamAccounts.confirmRelogin':'steamAccounts.confirmRestart';
    return `<div class="steam-account-confirmation" role="alert"><strong>${tr(title)}</strong>${target?`<div class="steam-account-confirm-account"><strong>${esc(target.personaName||target.accountName)}</strong><small>${esc(target.accountName)}</small></div>`:''}<p>${tr(body)}</p><code class="steam-account-confirm-target">${esc(this.confirmationPath)}</code><div><button id="steam-account-cancel" class="button secondary" data-steam-account-action="cancel" ${this.busy?'disabled':''}>${tr('common.cancel')}</button><button id="steam-account-confirm" class="button primary" data-steam-account-action="confirm" ${blocked?'disabled':''}>${tr(this.busy?'steamAccounts.working':'steamAccounts.confirm')}</button></div></div>`;
  }
  toggle(){
    if(this.busy||this.options.isAppBusy())return;
    if(this.open){this.close();return;}
    this.open=true;this.confirmation=null;this.confirmationAccount=null;this.changed();
    this.ownerDocument?.getElementById('steam-account-menu')?.focus();
    void this.refresh();
  }
  close(){this.open=false;this.confirmation=null;this.confirmationAccount=null;this.changed();this.ownerDocument?.getElementById('steam-account-button')?.focus();}
  async refresh(){
    if(this.loading||this.busy)return;
    this.resetForPath();const path=this.options.getSteamPath(),request=++this.request;
    this.loading=true;this.error=null;this.changed();
    try{const data=await this.api.state(path);if(request===this.request&&path===this.options.getSteamPath())this.data=data;}
    catch(error){if(request===this.request)this.error=this.errorNotice(error);}
    finally{if(request===this.request){this.loading=false;this.changed();}}
  }
  ask(action:SteamAccountAction,steamId?:string){
    if(this.busy||this.options.isAppBusy()||this.loading||!(this.data?.steamRoot||this.options.getSteamPath()))return;
    const target=action==='switchAccount'?this.data?.accounts.find(account=>account.steamId===steamId):null;
    if(action==='switchAccount'&&(!target||!/^\d{1,20}$/.test(target.steamId)||target.isCurrent||target.steamId===this.data?.currentSteamId))return;
    this.confirmationPath=this.options.getSteamPath()||this.data?.steamRoot||null;
    this.confirmationAccount=target?{...target}:null;
    this.confirmation=action;this.error=null;this.receipt=null;this.changed();
    this.ownerDocument?.getElementById('steam-account-cancel')?.focus();
  }
  async confirm(){
    const action=this.confirmation,target=this.confirmationAccount;
    if(!action||this.busy||this.options.isAppBusy()||this.loading)return;
    const path=this.options.getSteamPath()||this.data?.steamRoot||null;
    if(!path||path!==this.confirmationPath){++this.request;this.loadedPath=this.options.getSteamPath();this.data=null;this.loading=false;this.confirmation=null;this.confirmationAccount=null;this.error={messageKey:'steamAccounts.targetChanged',detail:''};this.changed();return;}
    this.busy=true;this.error=null;this.receipt=null;this.changed();
    let completed=false,nativeRequested=false,beforeActionStarted=false;
    try{
      if(action==='switchAccount'){
        if(!target)throw {messageKey:'steamAccounts.accountChanged',detail:''};
        const latest=await this.api.state(path);
        // Closing/changing this confirmation during a read cancels before side effects.
        if(this.confirmation!==action||this.confirmationAccount!==target)return;
        if(this.options.isAppBusy())throw {messageKey:'steamAccounts.operationFailed',detail:'Another application operation started before account selection'};
        if((this.options.getSteamPath()||this.data?.steamRoot||null)!==path||!sameSteamPath(latest.steamRoot,path))throw {messageKey:'steamAccounts.targetChanged',detail:''};
        this.data=latest;
        const saved=latest.accounts.find(account=>account.steamId===target.steamId);
        if(!saved||saved.isCurrent||saved.steamId===latest.currentSteamId||saved.accountName!==target.accountName||saved.personaName!==target.personaName)throw {messageKey:'steamAccounts.accountChanged',detail:''};
      }
      beforeActionStarted=true;
      this.options.onBeforeAction?.(action);this.changed();
      nativeRequested=true;
      const receipt=action==='switchAccount'?await this.api.switchAccount(path,target!.steamId):await this.api[action](path);
      if(!receipt.launchRequested)throw {messageKey:'steamAccounts.operationFailed',detail:'Steam did not request a launch'};
      if(action==='switchAccount'&&(receipt.operation!=='switchAccount'||receipt.selectedSteamId!==target!.steamId||!sameSteamPath(receipt.steamRoot,path)))throw {messageKey:'steamAccounts.selectionMismatch',detail:'Unexpected saved-account selection receipt'};
      this.receipt=receipt;this.data=null;this.confirmation=null;this.confirmationAccount=null;
      await this.options.onCompleted?.(receipt);
      this.options.onNotice?.(t(receipt.operation==='switchAccount'?'steamAccounts.switchRequested':receipt.requiresNativeLogin?'steamAccounts.nativeLoginRequested':'steamAccounts.restartRequested'));
      completed=true;
    }catch(error){this.error=this.errorNotice(error);this.confirmation=null;this.confirmationAccount=null;if(nativeRequested&&action==='switchAccount')this.data=null;if(this.error.messageKey==='steamAccounts.targetChanged'){++this.request;this.loadedPath=this.options.getSteamPath();this.data=null;}}
    finally{this.busy=false;if(beforeActionStarted)this.options.onAfterAction?.();this.changed();}
    if(completed&&this.open)await this.refresh();
  }
  private outsideClick=(event:Event)=>{
    const target=event.target;
    if(this.open&&target instanceof Element&&!target.closest('.steam-account-controls'))this.close();
  };
  private keyDown=(event:KeyboardEvent)=>{
    if(!this.open)return;
    if(event.key==='Escape'){event.preventDefault();this.close();return;}
    if(event.key!=='Tab')return;
    const panel=this.ownerDocument?.getElementById('steam-account-menu');
    if(!panel)return;
    const items=Array.from(panel.querySelectorAll<HTMLElement>('button:not(:disabled),[tabindex="0"]'));
    const first=items[0],last=items.at(-1),focused=this.ownerDocument?.activeElement;
    if(event.shiftKey&&(focused===first||focused===panel)){event.preventDefault();last?.focus();}
    else if(!event.shiftKey&&focused===last){event.preventDefault();first?.focus();}
  };
  bind(root:HTMLElement){
    this.ownerDocument?.removeEventListener('click',this.outsideClick);
    this.ownerDocument?.removeEventListener('keydown',this.keyDown);
    this.ownerDocument=root.ownerDocument;
    this.ownerDocument.addEventListener('click',this.outsideClick);
    this.ownerDocument.addEventListener('keydown',this.keyDown);
    root.querySelectorAll<HTMLElement>('[data-steam-account-action]').forEach(button=>button.addEventListener('click',()=>{
      const action=button.dataset.steamAccountAction;
      if(action==='toggle')this.toggle();else if(action==='close')this.close();else if(action==='refresh')void this.refresh();
      else if(action==='restart'||action==='relogin')this.ask(action);else if(action==='switchAccount')this.ask(action,button.dataset.steamAccountId);else if(action==='confirm')void this.confirm();
      else if(action==='cancel'){this.confirmation=null;this.confirmationAccount=null;this.changed();}
    }));
    root.querySelector('#steam-account-button')?.addEventListener('keydown',event=>{
      const e=event as KeyboardEvent;if(!this.open&&(e.key==='ArrowUp'||e.key==='ArrowDown')){e.preventDefault();this.toggle();}
    });
  }
  dispose(){++this.request;this.ownerDocument?.removeEventListener('click',this.outsideClick);this.ownerDocument?.removeEventListener('keydown',this.keyDown);}
}
