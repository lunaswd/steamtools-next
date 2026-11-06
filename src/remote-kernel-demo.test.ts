// @vitest-environment happy-dom
import { expect, it, vi } from 'vitest';
vi.mock('@tauri-apps/api/core',()=>({invoke:vi.fn(),isTauri:()=>false}));
vi.mock('@tauri-apps/plugin-dialog',()=>({open:vi.fn()}));
import { detectKernelInstallation, getEnvironment } from './api';
import { dictionaries } from './i18n';
it('reports unknown hash agreement, not a mismatch, when the demo kernel is absent',async()=>{
 const installation=await detectKernelInstallation('C:\\Fixture\\Steam');
 expect(installation.status).toBe('notInstalled');
 expect(installation.installedSha256).toBeNull();
 expect(installation.hashMatch).toBeNull();
});
it('describes remote acquisition rather than bundled DLL installation in every language',()=>{
 for(const catalog of Object.values(dictionaries)){expect(catalog['kernel.managementDescription']).not.toMatch(/bundled|随附|隨附/);expect(catalog['kernel.releaseSourcePlaceholder']).toContain('GitHub');expect(catalog['kernel.releaseSourcePlaceholder']).not.toContain('GitLab');}
 expect(dictionaries.en['kernel.managementDescription']).toContain('release repository');
 expect(dictionaries['zh-CN']['kernel.managementDescription']).toContain('发布仓库');
});

it('has no default local source workspace in the management environment',async()=>{
 const environment=await getEnvironment();
 expect(environment.defaultKernelPath).toBeNull();
 expect(environment.defaultKernelArtifactPath).toBeNull();
});
