// @vitest-environment happy-dom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { setLocale } from './i18n';
import { AuthorizationController, type AuthorizationControllerApi } from './authorization';
import type { AuthorizationTicketSummary, AuthorizationSessionSummary, AuthorizationExportReceipt, AuthorizationImportReceipt } from './authorization-api';

const STEAM='C:\\Fixture\\Steam',SOURCE='C:\\Fixture\\tickets.txt',DEST='C:\\Fixture\\Export\\tickets.txt';
const OWNER='76561197960265729',OTHER='76561197960265730';
const session:AuthorizationSessionSummary={steamRoot:STEAM,appId:42,steamId:OWNER,connected:true,subscribed:true};
const summary:AuthorizationTicketSummary={appId:42,steamId:OWNER,appTicketBytes:32,eTicketBytes:16,sha256:'a'.repeat(64),sourcePath:SOURCE,appIdOffset:16,steamIdOffset:8,validationScope:'fileStructureAndDeclaredIdentity'};
const exported:AuthorizationExportReceipt={summary:{...summary,sourcePath:DEST},outputPath:DEST,replaced:false,runtimeAuthorizationVerified:false};
const imported:AuthorizationImportReceipt={summary,registryPath:'Software\\Valve\\Steam\\Apps\\42',replaced:false,writeVerified:true,runtimeAuthorizationVerified:false};
let controller:AuthorizationController,root:HTMLElement,path:string|null,running:boolean|undefined,appBusy:boolean,api:AuthorizationControllerApi;
let onNotice=vi.fn(),onBefore=vi.fn(),onAfter=vi.fn();
const render=()=>{root.innerHTML=controller.markup();controller.bind(root)};
const el=<T extends HTMLElement=HTMLElement>(selector:string)=>root.querySelector<T>(selector)!;
const click=(selector:string)=>el(selector).click();
async function verify(){controller.setAppId('42');await controller.verify();expect(controller.session).toEqual(session)}
function deferred<T>(){let resolve!:(value:T)=>void;const promise=new Promise<T>(r=>{resolve=r});return {promise,resolve}}

beforeEach(()=>{
  setLocale('en');document.body.innerHTML='<div id="root"></div>';root=document.getElementById('root')!;path=STEAM;running=true;appBusy=false;onNotice=vi.fn();onBefore=vi.fn();onAfter=vi.fn();
  api={session:vi.fn().mockResolvedValue(structuredClone(session)),inspect:vi.fn().mockResolvedValue(structuredClone(summary)),extract:vi.fn().mockResolvedValue(structuredClone(exported)),import:vi.fn().mockResolvedValue(structuredClone(imported)),chooseSource:vi.fn().mockResolvedValue(SOURCE),chooseDestination:vi.fn().mockResolvedValue(DEST)};
  controller=new AuthorizationController({getSteamPath:()=>path,isSteamRunning:()=>running,isAppBusy:()=>appBusy,onChange:render,onBeforeAction:onBefore,onAfterAction:onAfter,onNotice},api);render();
});
afterEach(()=>{controller.dispose();document.body.innerHTML=''});

describe('private authorization metadata interface',()=>{
  it('does not collect tickets or account data at page startup',()=>{
    expect(api.session).not.toHaveBeenCalled();expect(api.inspect).not.toHaveBeenCalled();expect(api.extract).not.toHaveBeenCalled();expect(api.import).not.toHaveBeenCalled();
    expect(root.textContent).toContain('No ticket bytes');expect(root.querySelectorAll('input[type="password"],textarea')).toHaveLength(0);
    expect(el<HTMLButtonElement>('#authorization-extract').disabled).toBe(true);
  });
  it('never requests native authorization while Steam is stopped',async()=>{
    running=false;controller.setAppId('42');await controller.verify();await controller.ask('extract');
    expect(api.session).not.toHaveBeenCalled();expect(api.chooseDestination).not.toHaveBeenCalled();expect(root.textContent).toContain('Start Steam');expect(controller.session).toBeNull();
  });
  it('verifies only the selected AppID and does not trust missing ownership',async()=>{
    controller.setAppId('42');api.session=vi.fn().mockResolvedValue({...session,subscribed:false});await controller.verify();
    expect(api.session).toHaveBeenCalledWith(STEAM,42);expect(root.textContent).toContain('not confirmed');expect(el<HTMLButtonElement>('#authorization-extract').disabled).toBe(true);await controller.ask('extract');expect(api.extract).not.toHaveBeenCalled();
  });
  it('inspects a ticket file offline and exposes metadata, not its raw contents',async()=>{
    running=false;await controller.inspect();expect(api.inspect).toHaveBeenCalledWith(SOURCE);expect(root.textContent).toContain('AppTicket');expect(root.textContent).toContain(summary.sha256);
    expect(controller.appId).toBe('42');expect(controller.session).toBeNull();expect(root.querySelector('textarea')).toBeNull();expect(el<HTMLButtonElement>('#authorization-import').disabled).toBe(true);
  });
  it('extracts only after file choice and a second explicit confirmation',async()=>{
    await verify();await controller.ask('extract');expect(api.extract).not.toHaveBeenCalled();expect(root.textContent).toContain('do not share');
    await controller.confirm();expect(api.extract).toHaveBeenCalledExactlyOnceWith({steamPath:STEAM,appId:42,expectedSteamId:OWNER,destination:DEST,overwrite:false});expect(onNotice).toHaveBeenCalledWith('Private ticket file saved');expect(root.textContent).toContain('Steam and the game determine');expect(onBefore).toHaveBeenCalledTimes(1);expect(onAfter).toHaveBeenCalledTimes(1);
  });
  it('does nothing when the output file picker is canceled',async()=>{
    await verify();api.chooseDestination=vi.fn().mockResolvedValue(null);await controller.ask('extract');await controller.confirm();expect(api.extract).not.toHaveBeenCalled();expect(controller.confirmation).toBeNull();
  });
  it('imports only a matching owner/AppID and binds the full inspected-file hash',async()=>{
    await controller.inspect();await verify();await controller.ask('import');expect(api.import).not.toHaveBeenCalled();controller.overwrite=true;await controller.confirm();
    expect(api.import).toHaveBeenCalledExactlyOnceWith({steamPath:STEAM,appId:42,expectedSteamId:OWNER,sourcePath:SOURCE,expectedSha256:summary.sha256,overwrite:true});expect(root.textContent).toContain('Local ticket write verified');expect(root.textContent).toContain('device binding and license status');
  });
  it('refuses the UI import action for a different ticket account',async()=>{
    api.inspect=vi.fn().mockResolvedValue({...summary,steamId:OTHER});await controller.inspect();await verify();expect(el<HTMLButtonElement>('#authorization-import').disabled).toBe(true);await controller.ask('import');expect(api.import).not.toHaveBeenCalled();
  });
  it('checks the live account again at confirmation before acquiring ticket bytes',async()=>{
    await verify();await controller.ask('extract');api.session=vi.fn().mockResolvedValue({...session,steamId:OTHER});await controller.confirm();expect(api.extract).not.toHaveBeenCalled();expect(onNotice).not.toHaveBeenCalled();expect(root.textContent).toContain('changed');
  });
  it('does not use a stale target after the configured Steam folder changes',async()=>{
    await verify();await controller.ask('extract');path='C:\\Other\\Steam';await controller.confirm();expect(api.extract).not.toHaveBeenCalled();expect(controller.session).toBeNull();
  });
  it('rejects changed inspected metadata before import, even without editing raw text',async()=>{
    await controller.inspect();await verify();await controller.ask('import');controller.preview={...summary,sha256:'b'.repeat(64)};await controller.confirm();expect(api.import).not.toHaveBeenCalled();expect(root.textContent).toContain('changed');
  });
  it('keeps failed imports from being labeled as successful and drops error contents',async()=>{
    await controller.inspect();await verify();await controller.ask('import');api.import=vi.fn().mockRejectedValue({messageKey:'authorization.failed',detail:'PRIVATE_FIXTURE_AUTHORIZATION_BYTES'});await controller.confirm();expect(onNotice).not.toHaveBeenCalled();expect(controller.receipt).toBeNull();expect(root.textContent).not.toContain('PRIVATE_FIXTURE_AUTHORIZATION_BYTES');expect(onAfter).toHaveBeenCalledTimes(1);
  });
  it('requires successful local write verification and a matching returned identity',async()=>{
    await controller.inspect();await verify();await controller.ask('import');api.import=vi.fn().mockResolvedValue({...imported,writeVerified:false});await controller.confirm();expect(controller.receipt).toBeNull();expect(onNotice).not.toHaveBeenCalled();
  });
  it('does not allow an implicit overwrite that was not confirmed',async()=>{
    await verify();await controller.ask('extract');api.extract=vi.fn().mockResolvedValue({...exported,replaced:true});await controller.confirm();expect(controller.receipt).toBeNull();expect(onNotice).not.toHaveBeenCalled();
  });
  it('rejects a late successful response if the session was invalidated',async()=>{
    await verify();await controller.ask('extract');const pending=deferred<AuthorizationExportReceipt>();api.extract=vi.fn(()=>pending.promise);const operation=controller.confirm();await vi.waitFor(()=>expect(api.extract).toHaveBeenCalledTimes(1));controller.invalidate();pending.resolve(exported);await operation;expect(controller.receipt).toBeNull();expect(onNotice).not.toHaveBeenCalled();
  });
  it('blocks duplicate confirmed requests and other app work',async()=>{
    await verify();await controller.ask('extract');const pending=deferred<AuthorizationExportReceipt>();api.extract=vi.fn(()=>pending.promise);const operation=controller.confirm();await controller.confirm();await vi.waitFor(()=>expect(api.extract).toHaveBeenCalledTimes(1));pending.resolve(exported);await operation;
    appBusy=true;controller.setAppId('42');await controller.verify();expect(api.session).toHaveBeenCalledTimes(2);
  });
  it('drops invalid summary fields and escapes source filenames',async()=>{
    api.inspect=vi.fn().mockResolvedValue({...summary,appTicketBytes:Infinity});await controller.inspect();expect(controller.preview).toBeNull();
    const strange='C:\\Fixture\\<img src=x onerror=alert(1)>.txt';api.chooseSource=vi.fn().mockResolvedValue(strange);api.inspect=vi.fn().mockResolvedValue({...summary,sourcePath:strange});await controller.inspect();expect(root.querySelector('img')).toBeNull();expect(root.textContent).toContain('<img src=x');
  });
  it('preserves the account and AppID binding while the destination picker is open',async()=>{
    await verify();const pending=deferred<string|null>();api.chooseDestination=vi.fn(()=>pending.promise);const ask=controller.ask('extract');controller.setAppId('43');pending.resolve(DEST);await ask;expect(controller.confirmation).toBeNull();expect(api.extract).not.toHaveBeenCalled();
  });
  it('retains only whitelisted metadata even if an unexpected backend field is present',async()=>{
    api.inspect=vi.fn().mockResolvedValue({...summary,appTicket:'PRIVATE_FIXTURE_RAW_TICKET'});await controller.inspect();expect(controller.preview).not.toHaveProperty('appTicket');
    await verify();await controller.ask('extract');api.extract=vi.fn().mockResolvedValue({...exported,rawTicket:'PRIVATE_FIXTURE_RAW_TICKET'});await controller.confirm();expect(controller.receipt).not.toHaveProperty('rawTicket');expect(root.textContent).not.toContain('PRIVATE_FIXTURE_RAW_TICKET');
  });
});
