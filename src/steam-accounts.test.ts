// @vitest-environment happy-dom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { setLocale } from './i18n';
import { SteamAccountMenu, type SteamAccountMenuApi, type SteamAccountAction } from './steam-accounts';
import type { SteamAccountState, SteamLifecycleReceipt } from './types';

const state:SteamAccountState={steamRoot:'C:\\Fixture\\Steam',running:true,currentSteamId:'123',currentAccountName:'fixture',
  accounts:[{steamId:'123',accountName:'fixture',personaName:'Fixture user',isCurrent:true,lastLoginAt:100,canAutoLogin:true}],identitySource:'localPreferences',warnings:[]};
const receipt:SteamLifecycleReceipt={operation:'restart',steamRoot:state.steamRoot,previousRunning:true,launchRequested:true,processId:1234,requiresNativeLogin:false,selectedSteamId:'123',backupPath:null,warnings:[]};
let menu:SteamAccountMenu,root:HTMLElement,path:string|null,appBusy:boolean;
let api:SteamAccountMenuApi;
let onBefore=vi.fn((_action:SteamAccountAction)=>{}),onAfter=vi.fn(()=>{}),onCompleted=vi.fn((_receipt:SteamLifecycleReceipt)=>{}),onNotice=vi.fn((_message:string)=>{});
const el=<T extends HTMLElement=HTMLElement>(selector:string)=>root.querySelector<T>(selector)!;
const click=(selector:string)=>el(selector).click();
const render=()=>{root.innerHTML=`<aside class="sidebar"><div class="sidebar-bottom">${menu.markup()}</div></aside>`;menu.bind(root);};
beforeEach(()=>{
  setLocale('en');document.body.innerHTML='<div id="root"></div><button id="outside">Outside</button>';root=document.getElementById('root')!;path=state.steamRoot;appBusy=false;
  api={state:vi.fn().mockResolvedValue(structuredClone(state)),restart:vi.fn().mockResolvedValue(structuredClone(receipt)),relogin:vi.fn().mockResolvedValue({...receipt,operation:'relogin',requiresNativeLogin:true})};
  onBefore=vi.fn((_action:SteamAccountAction)=>{});onAfter=vi.fn(()=>{});onCompleted=vi.fn((_receipt:SteamLifecycleReceipt)=>{});onNotice=vi.fn((_message:string)=>{});
  menu=new SteamAccountMenu({getSteamPath:()=>path,isAppBusy:()=>appBusy,onChange:render,onBeforeAction:onBefore,onAfterAction:onAfter,onCompleted,onNotice},api);render();
});
afterEach(()=>{menu.dispose();document.body.innerHTML='';});
async function open(){click('#steam-account-button');await vi.waitFor(()=>expect(menu.loading).toBe(false));}

describe('lower-left Steam account management',()=>{
  it('renders in the lower-left rail without reading accounts at startup',()=>{
    expect(el('.sidebar-bottom #steam-account-button')).not.toBeNull();expect(root.querySelector('#steam-account-menu')).toBeNull();expect(api.state).not.toHaveBeenCalled();
  });
  it('reads saved accounts lazily and distinguishes preferences from online sign-in',async()=>{
    await open();expect(api.state).toHaveBeenCalledWith(path);expect(el('#steam-account-menu').textContent).toContain('Fixture user');expect(el('.steam-account-hint').textContent).toContain('online sign-in is confirmed in Steam');expect(el('.steam-account-selected').textContent).toBe('Selected');
    expect(root.querySelectorAll('input')).toHaveLength(0);expect(root.querySelector('[data-steam-account-action="switch"]')).toBeNull();
  });
  it('does not restart until the user explicitly confirms',async()=>{
    await open();click('#steam-account-restart');expect(api.restart).not.toHaveBeenCalled();expect(el('.steam-account-confirmation').textContent).toContain('games or downloads may be interrupted');click('#steam-account-cancel');expect(api.restart).not.toHaveBeenCalled();
  });
  it('restarts once and reports a launch request rather than a verified login',async()=>{
    await open();click('#steam-account-restart');click('#steam-account-confirm');await vi.waitFor(()=>expect(menu.busy).toBe(false));
    expect(api.restart).toHaveBeenCalledExactlyOnceWith(path);expect(onBefore).toHaveBeenCalledWith('restart');expect(onAfter).toHaveBeenCalledTimes(1);expect(onCompleted).toHaveBeenCalledWith(receipt);expect(onNotice).toHaveBeenCalledWith('Steam launch requested. Confirm the running state in Steam.');expect(el('.steam-account-result').textContent).toContain('launch requested');
  });
  it('routes re-login to native Steam and never collects passwords or Steam Guard',async()=>{
    await open();click('#steam-account-relogin');expect(api.relogin).not.toHaveBeenCalled();expect(el('.steam-account-confirmation').textContent).toContain('native sign-in/account selection');click('#steam-account-confirm');await vi.waitFor(()=>expect(menu.busy).toBe(false));
    expect(api.relogin).toHaveBeenCalledExactlyOnceWith(path);expect(el('.steam-account-result').textContent).toContain('Steam’s own window');expect(root.querySelectorAll('input')).toHaveLength(0);
  });
  it('prevents duplicate confirmed requests while work is in progress',async()=>{
    let resolve!:(result:SteamLifecycleReceipt)=>void;api.restart=vi.fn(()=>new Promise<SteamLifecycleReceipt>(r=>{resolve=r}));await open();menu.ask('restart');const first=menu.confirm();await menu.confirm();expect(api.restart).toHaveBeenCalledTimes(1);expect(el<HTMLButtonElement>('#steam-account-confirm').disabled).toBe(true);resolve(receipt);await first;
  });
  it('retains an operation error without claiming success or clearing it during refresh',async()=>{
    api.restart=vi.fn().mockRejectedValue({messageKey:'steamAccounts.operationFailed',detail:'Fixture timeout'});await open();menu.ask('restart');await menu.confirm();expect(el('.steam-account-error').textContent).toContain('Fixture timeout');expect(root.querySelector('.steam-account-result')).toBeNull();expect(onNotice).not.toHaveBeenCalled();expect(api.state).toHaveBeenCalledTimes(1);expect(onAfter).toHaveBeenCalledTimes(1);
  });
  it('never reports success when Steam launch was not requested',async()=>{
    api.restart=vi.fn().mockResolvedValue({...receipt,launchRequested:false});await open();menu.ask('restart');await menu.confirm();expect(el('.steam-account-error').textContent).toContain('Steam account operation failed');expect(onCompleted).not.toHaveBeenCalled();expect(onNotice).not.toHaveBeenCalled();
  });
  it('never renders an unknown backend message key as user-facing text',async()=>{
    api.restart=vi.fn().mockRejectedValue({messageKey:'unexpected.account.value',detail:''});await open();menu.ask('restart');await menu.confirm();expect(el('.steam-account-error').textContent).toContain('Steam account operation failed');expect(root.textContent).not.toContain('unexpected.account.value');
  });
  it('rejects a target folder changed after confirmation',async()=>{
    await open();menu.ask('relogin');path='C:\\Other\\Steam';await menu.confirm();expect(api.relogin).not.toHaveBeenCalled();expect(el('.steam-account-error').textContent).toContain('Steam folder changed');
  });
  it('drops a stale account response after the configured Steam path changes',async()=>{
    let resolve!:(result:SteamAccountState)=>void;api.state=vi.fn(()=>new Promise<SteamAccountState>(r=>{resolve=r}));menu.toggle();path='C:\\Other\\Steam';render();resolve(state);await Promise.resolve();await Promise.resolve();expect(menu.data).toBeNull();expect(root.textContent).not.toContain('Fixture user');
  });
  it('escapes non-secret display names rather than interpreting HTML',async()=>{
    api.state=vi.fn().mockResolvedValue({...state,accounts:[{...state.accounts[0],personaName:'<img src=x onerror=alert(1)>'}]});await open();expect(root.querySelector('img')).toBeNull();expect(el('.steam-saved-account-list').textContent).toContain('<img src=x');
  });
  it('closes on Escape or outside clicks and restores the trigger focus',async()=>{
    await open();document.dispatchEvent(new KeyboardEvent('keydown',{key:'Escape',bubbles:true}));expect(root.querySelector('#steam-account-menu')).toBeNull();expect(document.activeElement?.id).toBe('steam-account-button');await open();document.getElementById('outside')!.click();expect(root.querySelector('#steam-account-menu')).toBeNull();
  });
  it('keeps keyboard tab focus within the account panel',async()=>{
    await open();const close=el('#steam-account-close'),refresh=el('#steam-account-refresh');refresh.focus();refresh.dispatchEvent(new KeyboardEvent('keydown',{key:'Tab',bubbles:true}));expect(document.activeElement).toBe(close);close.dispatchEvent(new KeyboardEvent('keydown',{key:'Tab',shiftKey:true,bubbles:true}));expect(document.activeElement).toBe(refresh);
  });
  it('does not overlap another application operation',()=>{
    appBusy=true;render();expect(el<HTMLButtonElement>('#steam-account-button').disabled).toBe(true);menu.toggle();expect(menu.open).toBe(false);menu.ask('restart');expect(menu.confirmation).toBeNull();
  });
});
