import { beforeEach, describe, expect, it, vi } from 'vitest';
const core=vi.hoisted(()=>({invoke:vi.fn(),isTauri:vi.fn()}));
const dialog=vi.hoisted(()=>({open:vi.fn(),save:vi.fn()}));
vi.mock('@tauri-apps/api/core',()=>core);
vi.mock('@tauri-apps/plugin-dialog',()=>dialog);
import { authorizationSession,inspectAuthorizationTicket,extractAuthorizationTicket,importAuthorizationTicket,chooseAuthorizationSource,chooseAuthorizationDestination } from './authorization-api';
beforeEach(()=>{vi.clearAllMocks();core.isTauri.mockReturnValue(true);core.invoke.mockResolvedValue({});dialog.open.mockResolvedValue('C:\\Fixture\\tickets.txt');dialog.save.mockResolvedValue('C:\\Fixture\\Export\\tickets.txt')});
describe('private authorization IPC boundary',()=>{
  it('uses exact metadata commands with no frontend-owned ownership assertion',async()=>{
    await authorizationSession('C:\\Fixture\\Steam',42);await inspectAuthorizationTicket('C:\\Fixture\\tickets.txt');
    expect(core.invoke.mock.calls).toEqual([['authorization_session_command',{steamPath:'C:\\Fixture\\Steam',appId:42}],['inspect_authorization_ticket_command',{sourcePath:'C:\\Fixture\\tickets.txt'}]]);
  });
  it('sends only selected paths, app ID, account binding and inspection hash',async()=>{
    const selected={steamPath:'C:\\Fixture\\Steam',appId:42,expectedSteamId:'76561197960265729',overwrite:false};
    await extractAuthorizationTicket({...selected,destination:'C:\\Fixture\\Export\\tickets.txt'});await importAuthorizationTicket({...selected,sourcePath:'C:\\Fixture\\tickets.txt',expectedSha256:'a'.repeat(64)});
    const payloads=core.invoke.mock.calls.map(call=>call[1]);expect(payloads.every(value=>!('appTicket'in value)&&!('eTicket'in value)&&!('password'in value)&&!('subscribed'in value))).toBe(true);
  });
  it('limits local dialogs to ticket text files and lets the user choose the destination',async()=>{
    expect(await chooseAuthorizationSource()).toContain('tickets.txt');expect(await chooseAuthorizationDestination()).toContain('Export');expect(dialog.open).toHaveBeenCalledWith(expect.objectContaining({multiple:false,directory:false,filters:[{name:'Steam ticket text',extensions:['txt']}]}));expect(dialog.save).toHaveBeenCalledWith(expect.objectContaining({defaultPath:'tickets.txt'}));
  });
  it('does not emulate successful native extraction or import in browser previews',async()=>{
    core.isTauri.mockReturnValue(false);await expect(authorizationSession('C:\\Fixture\\Steam',42)).rejects.toMatchObject({messageKey:'authorization.desktopOnly'});await expect(chooseAuthorizationSource()).rejects.toMatchObject({messageKey:'authorization.desktopOnly'});expect(core.invoke).not.toHaveBeenCalled();expect(dialog.open).not.toHaveBeenCalled();
  });
});
