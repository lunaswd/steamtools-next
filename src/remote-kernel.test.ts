import { describe, expect, it } from 'vitest';
import { DEFAULT_KERNEL_RELEASE_SOURCE, migrateKernelSettings } from './remote-kernel';

describe('fixed kernel settings migration',()=>{
 it('drops custom source and local artifact paths while retaining Steam preferences',()=>{
   const migrated=migrateKernelSettings({kernelPath:'C:\\Fixture\\source',kernelArtifactPath:'C:\\Fixture\\artifact',steamPath:'C:\\Fixture\\Steam'});
   expect(migrated).toMatchObject({kernelPath:null,kernelArtifactPath:null,kernelReleaseSource:DEFAULT_KERNEL_RELEASE_SOURCE,injectionMethod:'dwmapi',steamPath:'C:\\Fixture\\Steam'});
 });
 it('replaces every historical repository and injection entry with the built-in defaults',()=>{
   for(const kernelReleaseSource of [undefined,null,'','https://github.com/fixture/kernel','https://gitlab.com/fixture/kernel']){
     const migrated=migrateKernelSettings({kernelReleaseSource,injectionMethod:'version'});
     expect(migrated.kernelReleaseSource).toBe(DEFAULT_KERNEL_RELEASE_SOURCE);
     expect(migrated.injectionMethod).toBe('dwmapi');
   }
 });
 it('does not accept a caller-supplied default repository override',()=>{
   expect(migrateKernelSettings({},'https://github.com/fixture/kernel').kernelReleaseSource).toBe(DEFAULT_KERNEL_RELEASE_SOURCE);
 });
});
