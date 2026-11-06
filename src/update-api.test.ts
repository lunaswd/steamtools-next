// @vitest-environment happy-dom
import { beforeEach, expect, it, vi } from 'vitest';
const mocks=vi.hoisted(()=>({invoke:vi.fn(),desktop:true}));
vi.mock('@tauri-apps/api/core',()=>({invoke:mocks.invoke,isTauri:()=>mocks.desktop}));
import { checkUpdates, installToolUpdate } from './update-api';
beforeEach(()=>{mocks.invoke.mockReset();mocks.desktop=true});
it('checks only the fixed native update command, without passing library, account or repository information',async()=>{
  mocks.invoke.mockResolvedValue({});await checkUpdates('C:\\Fixture\\Steam');
  expect(mocks.invoke).toHaveBeenCalledWith('check_updates_command',{steamPath:'C:\\Fixture\\Steam'});
});
it('applies a user-selected version without accepting arbitrary paths or URLs',async()=>{
  mocks.invoke.mockResolvedValue({prepared:true,restarting:true});await installToolUpdate('0.3.1');
  expect(mocks.invoke).toHaveBeenCalledWith('install_tool_update_command',{expectedVersion:'0.3.1'});
});
it('makes no writes or network requests in a browser preview',async()=>{
  mocks.desktop=false;const result=await checkUpdates(null);expect(result.tool.status).toBe('unknown');
  await expect(installToolUpdate('0.3.1')).rejects.toMatchObject({messageKey:'updates.desktopOnly'});expect(mocks.invoke).not.toHaveBeenCalled();
});
