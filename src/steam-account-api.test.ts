import { beforeEach, describe, expect, it, vi } from 'vitest';
const api=vi.hoisted(()=>({invoke:vi.fn(),isTauri:vi.fn()}));
vi.mock('@tauri-apps/api/core',()=>api);
import { reloginSteam, restartSteam, steamAccountState } from './steam-account-api';
beforeEach(()=>{vi.clearAllMocks();api.isTauri.mockReturnValue(true);api.invoke.mockResolvedValue({});});
describe('Steam account IPC boundary',()=>{
  it('requests the non-secret account state with the selected Steam folder',async()=>{await steamAccountState('C:\\Fixture\\Steam');expect(api.invoke).toHaveBeenCalledWith('steam_account_state_command',{steamPath:'C:\\Fixture\\Steam'});});
  it('calls exact restart and re-login commands without credential arguments',async()=>{await restartSteam('C:\\Fixture\\Steam');await reloginSteam('C:\\Fixture\\Steam');expect(api.invoke.mock.calls).toEqual([['restart_steam_command',{steamPath:'C:\\Fixture\\Steam'}],['relogin_steam_command',{steamPath:'C:\\Fixture\\Steam'}]]);});
  it('does not pretend browser previews perform a restart or sign-in',async()=>{api.isTauri.mockReturnValue(false);await expect(restartSteam(null)).rejects.toMatchObject({messageKey:'steamAccounts.previewOnly'});await expect(reloginSteam(null)).rejects.toMatchObject({messageKey:'steamAccounts.previewOnly'});expect(api.invoke).not.toHaveBeenCalled();expect(await steamAccountState(null)).toMatchObject({identitySource:'demo',running:false,accounts:[]});});
});
