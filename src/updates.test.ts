// @vitest-environment happy-dom
import { beforeEach, afterEach, expect, it, vi } from 'vitest';
import { UpdateController } from './updates';
import type { UpdateControllerApi } from './updates';
import type { UpdatesState, ToolUpdateReceipt } from './update-api';
import { setLocale } from './i18n';

const current=():UpdatesState=>({tool:{status:'current',currentVersion:'0.3.0',latestVersion:'0.3.0',releaseTag:'v0.3.0'},kernel:{status:'current',currentVersion:'1.5.4.1',latestVersion:'1.5.4.1'},checkedAt:123});
const available=():UpdatesState=>({tool:{status:'available',currentVersion:'0.3.0',latestVersion:'0.3.1',releaseTag:'v0.3.1'},kernel:{status:'available',currentVersion:'1.5.4.1',latestVersion:'1.5.4.2',releaseTag:'v1.5.4.2',latestSha256:'a'.repeat(64)},checkedAt:456});
function deferred<T>(){let resolve!:(value:T)=>void;let reject!:(reason:unknown)=>void;const promise=new Promise<T>((yes,no)=>{resolve=yes;reject=no});return {promise,resolve,reject}}
let controller:UpdateController;
let api:UpdateControllerApi;
let beforeApply=vi.fn<()=>void>(),applyFailed=vi.fn<()=>void>();
let appBusy=false;
let root:HTMLElement;
function setup(){
  api={check:vi.fn(async()=>current()),install:vi.fn(async()=>({prepared:true,restarting:true}))};
  beforeApply=vi.fn<()=>void>();applyFailed=vi.fn<()=>void>();appBusy=false;
  document.body.innerHTML='<div id="fixture"></div>';root=document.getElementById('fixture')!;
  const render=()=>{root.innerHTML=controller.menuButton()+controller.markup();controller.bind(root)};
  controller=new UpdateController({getSteamPath:()=> 'C:\\Fixture\\Steam',getAppVersion:()=> '0.3.0',isAppBusy:()=>appBusy,onChange:render,onBeforeApply:beforeApply,onApplyFailed:applyFailed},api);
  render();
}
function click(selector:string){const button=root.querySelector<HTMLButtonElement>(selector);expect(button,selector).not.toBeNull();button!.click()}
beforeEach(()=>{setLocale('en');setup()});
afterEach(()=>controller.dispose());

it('checks release metadata in the background without opening a current-version modal or applying anything',async()=>{
  const wait=deferred<UpdatesState>();vi.mocked(api.check).mockReturnValueOnce(wait.promise);
  const promise=controller.check(true);
  expect(controller.checking).toBe(true);expect(controller.open).toBe(false);expect(controller.phase).toBe('idle');
  expect(api.check).toHaveBeenCalledWith('C:\\Fixture\\Steam');expect(api.install).not.toHaveBeenCalled();
  wait.resolve(current());await promise;
  expect(controller.checking).toBe(false);expect(controller.open).toBe(false);
});
it('shows one available-update notification at startup and keeps kernel management as an instruction, not another action',async()=>{
  vi.mocked(api.check).mockResolvedValue(available());await controller.check(true);
  expect(controller.open).toBe(true);expect(root.querySelector('[role="dialog"]')).not.toBeNull();
  expect(controller.kernelBanner()).toContain('1.5.4.2');expect(controller.kernelBanner()).not.toContain('<button');
  controller.close();expect(controller.open).toBe(false);await controller.check(true);
  expect(api.check).toHaveBeenCalledTimes(1);expect(controller.open).toBe(false);expect(api.install).not.toHaveBeenCalled();
});
it('does not reopen an automatic check when the user already closed its pending dialog',async()=>{
  const wait=deferred<UpdatesState>();vi.mocked(api.check).mockReturnValue(wait.promise);
  const promise=controller.check(true);controller.openDialog();controller.close();wait.resolve(available());await promise;
  expect(controller.open).toBe(false);
});
it('manual check always refreshes metadata and does not silently reuse the startup result',async()=>{
  await controller.check(true);await controller.openAndCheck();
  expect(api.check).toHaveBeenCalledTimes(2);expect(controller.open).toBe(true);
});
it('requires explicit confirmation, prepares the selected version, and leaves restart to the native helper',async()=>{
  vi.mocked(api.check).mockResolvedValue(available());await controller.check(true);
  click('#updates-install');expect(controller.confirmation).toBe(true);expect(api.install).not.toHaveBeenCalled();
  const wait=deferred<ToolUpdateReceipt>();vi.mocked(api.install).mockReturnValue(wait.promise);
  const promise=controller.confirmInstall();expect(controller.phase).toBe('preparing');expect(beforeApply).toHaveBeenCalledTimes(1);
  expect(api.install).toHaveBeenCalledWith('0.3.1');controller.close();expect(controller.open).toBe(true);
  expect(root.querySelector('#updates-close')?.hasAttribute('disabled')).toBe(true);
  wait.resolve({prepared:true,restarting:true});await promise;
  expect(controller.phase).toBe('restarting');expect(applyFailed).not.toHaveBeenCalled();
});
it('returns to usable UI after a download/helper error and permits an explicit retry',async()=>{
  vi.mocked(api.check).mockResolvedValue(available());await controller.check(true);controller.askToInstall();
  vi.mocked(api.install).mockRejectedValueOnce({messageKey:'updates.failed',detail:'Fixture checksum mismatch'});
  await controller.confirmInstall();expect(controller.phase).toBe('idle');expect(applyFailed).toHaveBeenCalledTimes(1);
  expect(root.textContent).toContain('Fixture checksum mismatch');expect(root.querySelector('#updates-install')).not.toBeNull();
  controller.askToInstall();await controller.confirmInstall();expect(api.install).toHaveBeenCalledTimes(2);
});
it('never reports a prepared-only or unsuccessful helper response as a restart',async()=>{
  vi.mocked(api.check).mockResolvedValue(available());await controller.check(true);controller.askToInstall();
  vi.mocked(api.install).mockResolvedValue({prepared:true,restarting:false});await controller.confirmInstall();
  expect(controller.phase).toBe('idle');expect(controller.error).not.toBeNull();expect(applyFailed).toHaveBeenCalledTimes(1);
});
it('does not initiate a tool update during another application operation',async()=>{
  vi.mocked(api.check).mockResolvedValue(available());await controller.check(true);appBusy=true;controller.askToInstall();
  expect(controller.confirmation).toBe(false);await controller.confirmInstall();expect(api.install).not.toHaveBeenCalled();
});
it('keeps the app usable after failed automatic update checks instead of opening a repeated error dialog',async()=>{
  vi.mocked(api.check).mockRejectedValue({messageKey:'updates.failed',detail:'Fixture unavailable'});await controller.check(true);
  expect(controller.open).toBe(false);expect(controller.phase).toBe('idle');expect(controller.checking).toBe(false);
  await controller.openAndCheck();expect(root.textContent).toContain('Fixture unavailable');expect(api.install).not.toHaveBeenCalled();
});
it('escapes all update version and technical evidence fields',async()=>{
  const value=available();value.tool.latestVersion='<img src=x onerror=alert(1)>';value.kernel.error={detail:'<script>secret</script>'};
  vi.mocked(api.check).mockResolvedValue(value);await controller.check(true);
  expect(root.querySelector('img')).toBeNull();expect(root.querySelector('script')).toBeNull();expect(root.textContent).toContain('<img src=x');
});
it('dismisses with Escape and traps keyboard focus inside the visible dialog',async()=>{
  vi.mocked(api.check).mockResolvedValue(available());await controller.check(true);
  const dialog=root.querySelector<HTMLElement>('#update-dialog')!;dialog.focus();document.dispatchEvent(new KeyboardEvent('keydown',{key:'Tab',bubbles:true,cancelable:true}));
  expect(dialog.contains(document.activeElement)).toBe(true);
  document.dispatchEvent(new KeyboardEvent('keydown',{key:'Escape',bubbles:true,cancelable:true}));expect(controller.open).toBe(false);
});
it('clears a kernel notification only after the installed DLL hash matches the available release',async()=>{
  vi.mocked(api.check).mockResolvedValue(available());await controller.check(true);
  controller.kernelInstalled('1.5.4.2','b'.repeat(64));expect(controller.kernelBanner()).not.toBe('');
  controller.kernelInstalled('1.5.4.2','a'.repeat(64));expect(controller.kernelBanner()).toBe('');
});
