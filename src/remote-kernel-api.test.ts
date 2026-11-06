// @vitest-environment happy-dom
import { beforeEach, expect, it, vi } from 'vitest';
import type { RemoteKernelInstallRequest, Settings } from './types';
const invoke=vi.hoisted(()=>vi.fn());
vi.mock('@tauri-apps/api/core',()=>({invoke,isTauri:()=>true}));
vi.mock('@tauri-apps/plugin-dialog',()=>({open:vi.fn()}));
import { installKernel, loadSettings, saveSettings } from './api';
import * as managementApi from './api';
import { DEFAULT_KERNEL_RELEASE_SOURCE } from './remote-kernel';
const request:RemoteKernelInstallRequest={steamRoot:'C:\\Fixture\\Steam',installExampleConfig:true,backupRoot:null,dryRun:false};
beforeEach(()=>invoke.mockReset());
it('installs through the fixed remote command with no source or injection fields',async()=>{
 invoke.mockResolvedValue({release:{version:'1.5.4.1'},receipt:{}});
 await installKernel(request);
 expect(invoke).toHaveBeenLastCalledWith('install_remote_kernel_command',{request});
 const args=invoke.mock.calls[0][1].request;
 for(const key of ['source','artifactRoot','injectionMethod'])expect(args).not.toHaveProperty(key);
});
it('migrates obsolete source settings and entry overrides on load and save',async()=>{
 const old={kernelPath:'C:\\Fixture\\old-source',kernelArtifactPath:'C:\\Fixture\\kernel',kernelReleaseSource:'https://github.com/fixture/kernel',injectionMethod:'version'} as Settings;
 invoke.mockResolvedValueOnce(old);
 const settings=await loadSettings();
 expect(settings).toMatchObject({kernelPath:null,kernelArtifactPath:null,kernelReleaseSource:DEFAULT_KERNEL_RELEASE_SOURCE,injectionMethod:'dwmapi'});
 await saveSettings(old);
 expect(invoke).toHaveBeenLastCalledWith('save_settings_command',{settings:{kernelPath:null,kernelArtifactPath:null,kernelReleaseSource:DEFAULT_KERNEL_RELEASE_SOURCE,injectionMethod:'dwmapi'}});
});
it('exposes only kernel install and uninstall rather than retired advanced actions',()=>{
 for(const method of ['inspectKernel','compareKernels','detectInstalledKernel','planKernelInstall','setKernelEnabled','switchKernelInjection'])expect(managementApi).not.toHaveProperty(method);
 expect(managementApi.installKernel).toBeTypeOf('function');
 expect(managementApi.uninstallKernel).toBeTypeOf('function');
});
