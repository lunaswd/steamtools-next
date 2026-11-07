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
  api={state:vi.fn().mockResolvedValue(structuredClone(state)),restart:vi.fn().mockResolvedValue(structuredClone(receipt)),relogin:vi.fn().mockResolvedValue({...receipt,operation:'relogin',requiresNativeLogin:true}),switchAccount:vi.fn().mockResolvedValue({...receipt,operation:'switchAccount',selectedSteamId:'76561197960265730'})};
  onBefore=vi.fn((_action:SteamAccountAction)=>{});onAfter=vi.fn(()=>{});onCompleted=vi.fn((_receipt:SteamLifecycleReceipt)=>{});onNotice=vi.fn((_message:string)=>{});
  menu=new SteamAccountMenu({getSteamPath:()=>path,isAppBusy:()=>appBusy,onChange:render,onBeforeAction:onBefore,onAfterAction:onAfter,onCompleted,onNotice},api);render();
});
afterEach(()=>{menu.dispose();document.body.innerHTML='';});
async function open(){click('#steam-account-button');await vi.waitFor(()=>expect(menu.loading).toBe(false));}
const target={steamId:'76561197960265730',accountName:'fixture-two',personaName:'Fixture Two',isCurrent:false,lastLoginAt:90,canAutoLogin:true};
const switchSelector='[data-steam-account-action="switchAccount"][data-steam-account-id="76561197960265730"]';
async function openWithTarget(){api.state=vi.fn().mockResolvedValue({...state,accounts:[...state.accounts,target]});await open()}

describe('lower-left Steam account management',()=>{
  it('switches only after confirmation and validates the saved-account selection receipt',async()=>{
    await openWithTarget();click(switchSelector);expect(api.switchAccount).not.toHaveBeenCalled();
    expect(el('.steam-account-confirmation').textContent).toContain('games or downloads may be interrupted');
    click('#steam-account-confirm');await vi.waitFor(()=>expect(menu.busy).toBe(false));
    expect(api.switchAccount).toHaveBeenCalledExactlyOnceWith(path,target.steamId);
    expect(onBefore).toHaveBeenCalledWith('switchAccount');expect(onCompleted).toHaveBeenCalledWith(expect.objectContaining({operation:'switchAccount',selectedSteamId:target.steamId}));
    expect(el('.steam-account-result').textContent).toContain('Confirm the active account in Steam’s own window');
    expect(el('.steam-account-result').textContent).not.toContain('signed in successfully');
  });
  it('does not switch the current account or a missing target',async()=>{
    await openWithTarget();click('[data-steam-account-action="switchAccount"][data-steam-account-id="123"]');
    expect(menu.confirmation).toBeNull();menu.ask('switchAccount','999');expect(menu.confirmation).toBeNull();
    menu.ask('switchAccount','123');expect(menu.confirmation).toBeNull();expect(api.switchAccount).not.toHaveBeenCalled();
  });
  it('rechecks the target before switching and rejects a removed account',async()=>{
    await openWithTarget();menu.ask('switchAccount',target.steamId);api.state=vi.fn().mockResolvedValue(state);await menu.confirm();
    expect(api.switchAccount).not.toHaveBeenCalled();expect(onBefore).not.toHaveBeenCalled();
    expect(el('.steam-account-error').textContent).toContain('no longer available');
    expect(root.querySelector(switchSelector)).toBeNull();
  });
  it('rejects a saved-account identity changed after confirmation',async()=>{
    await openWithTarget();menu.ask('switchAccount',target.steamId);api.state=vi.fn().mockResolvedValue({...state,accounts:[...state.accounts,{...target,accountName:'changed-fixture'}]});await menu.confirm();
    expect(api.switchAccount).not.toHaveBeenCalled();expect(el('.steam-account-error').textContent).toContain('account changed');
  });
  it('does not restart a target that became current after confirmation',async()=>{
    await openWithTarget();menu.ask('switchAccount',target.steamId);api.state=vi.fn().mockResolvedValue({...state,currentSteamId:target.steamId,accounts:[...state.accounts,{...target,isCurrent:true}]});await menu.confirm();
    expect(api.switchAccount).not.toHaveBeenCalled();expect(el<HTMLButtonElement>(switchSelector).disabled).toBe(true);
  });
  it('rejects a target folder changed during the final saved-account read',async()=>{
    await openWithTarget();menu.ask('switchAccount',target.steamId);
    let resolve!:(result:SteamAccountState)=>void;api.state=vi.fn(()=>new Promise<SteamAccountState>(r=>{resolve=r}));const pending=menu.confirm();
    path='C:\\Other\\Steam';resolve({...state,accounts:[...state.accounts,target]});await pending;
    expect(api.switchAccount).not.toHaveBeenCalled();expect(onBefore).not.toHaveBeenCalled();expect(el('.steam-account-error').textContent).toContain('Steam folder changed');
  });
  it('cancels a switch before side effects if its confirmation was closed while rereading',async()=>{
    await openWithTarget();menu.ask('switchAccount',target.steamId);
    let resolve!:(result:SteamAccountState)=>void;api.state=vi.fn(()=>new Promise<SteamAccountState>(r=>{resolve=r}));const pending=menu.confirm();menu.close();resolve({...state,accounts:[...state.accounts,target]});await pending;
    expect(api.switchAccount).not.toHaveBeenCalled();expect(onBefore).not.toHaveBeenCalled();expect(menu.busy).toBe(false);
  });
  it('does not overwrite another operation busy state during the saved-account recheck',async()=>{
    await openWithTarget();menu.ask('switchAccount',target.steamId);
    let resolve!:(result:SteamAccountState)=>void;api.state=vi.fn(()=>new Promise<SteamAccountState>(r=>{resolve=r}));const pending=menu.confirm();
    appBusy=true;resolve({...state,accounts:[...state.accounts,target]});await pending;
    expect(api.switchAccount).not.toHaveBeenCalled();expect(onBefore).not.toHaveBeenCalled();expect(onAfter).not.toHaveBeenCalled();expect(appBusy).toBe(true);
  });
  it('does not claim switch success for a mismatched returned account',async()=>{
    await openWithTarget();menu.ask('switchAccount',target.steamId);api.switchAccount=vi.fn().mockResolvedValue({...receipt,operation:'switchAccount',selectedSteamId:'123'});await menu.confirm();
    expect(onCompleted).not.toHaveBeenCalled();expect(onNotice).not.toHaveBeenCalled();expect(root.querySelector('.steam-account-result')).toBeNull();
    expect(el('.steam-account-error').textContent).toContain('account selection was not confirmed');
  });
  it('keeps exactly one switch request active and disables other saved-account buttons',async()=>{
    await openWithTarget();menu.ask('switchAccount',target.steamId);let resolve!:(result:SteamLifecycleReceipt)=>void;
    api.switchAccount=vi.fn(()=>new Promise<SteamLifecycleReceipt>(r=>{resolve=r}));const first=menu.confirm();
    await vi.waitFor(()=>expect(api.switchAccount).toHaveBeenCalledTimes(1));await menu.confirm();
    expect(el<HTMLButtonElement>(switchSelector).disabled).toBe(true);expect(api.switchAccount).toHaveBeenCalledTimes(1);
    resolve({...receipt,operation:'switchAccount',selectedSteamId:target.steamId});await first;
  });
  it('gives saved-account switching a keyboard-accessible label and focus loop',async()=>{
    await openWithTarget();const switchButton=el<HTMLButtonElement>(switchSelector),close=el('#steam-account-close');
    expect(switchButton.getAttribute('aria-label')).toBe('Switch to Fixture Two');switchButton.focus();switchButton.dispatchEvent(new KeyboardEvent('keydown',{key:'Tab',bubbles:true}));
    expect(document.activeElement).toBe(close);close.dispatchEvent(new KeyboardEvent('keydown',{key:'Tab',shiftKey:true,bubbles:true}));expect(document.activeElement).toBe(switchButton);
  });
  it('escapes the selected account in switch confirmation and labels',async()=>{
    api.state=vi.fn().mockResolvedValue({...state,accounts:[...state.accounts,{...target,personaName:'<img src=x onerror=alert(1)>',accountName:'fixture&two'}]});await open();click(switchSelector);
    expect(root.querySelector('img')).toBeNull();expect(el('.steam-account-confirm-account').textContent).toContain('<img src=x');expect(el('.steam-account-confirm-account').textContent).toContain('fixture&two');
  });
  it('offers an explicit switch for a non-current saved account',async()=>{
    api.state=vi.fn().mockResolvedValue({...state,accounts:[...state.accounts,{steamId:'76561197960265730',accountName:'fixture-two',personaName:'Fixture Two',isCurrent:false,lastLoginAt:90,canAutoLogin:true}]});
    await open();const button=el<HTMLButtonElement>('[data-steam-account-action="switchAccount"][data-steam-account-id="76561197960265730"]');
    expect(button).not.toBeNull();expect(button.disabled).toBe(false);button.click();
    expect(el('.steam-account-confirmation').textContent).toContain('Fixture Two');
    expect(el('.steam-account-confirmation').textContent).toContain('fixture-two');
  });
  it('renders in the lower-left rail without reading accounts at startup',()=>{
    expect(el('.sidebar-bottom #steam-account-button')).not.toBeNull();expect(root.querySelector('#steam-account-menu')).toBeNull();expect(api.state).not.toHaveBeenCalled();
  });
  it('reads saved accounts lazily and distinguishes preferences from online sign-in',async()=>{
    await open();expect(api.state).toHaveBeenCalledWith(path);expect(el('#steam-account-menu').textContent).toContain('Fixture user');expect(el('.steam-account-hint').textContent).toContain('online sign-in is confirmed in Steam');expect(el('.steam-account-selected').textContent).toBe('Selected');
    expect(root.querySelectorAll('input')).toHaveLength(0);expect(el<HTMLButtonElement>('[data-steam-account-action="switchAccount"]').disabled).toBe(true);
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
