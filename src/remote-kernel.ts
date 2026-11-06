import type { Settings } from './types';

export const DEFAULT_KERNEL_RELEASE_SOURCE = 'https://github.com/lunaswd/steamtools-next-kernel';

/** Fixed release repository and injection entry; legacy local paths are never inputs. */
export function migrateKernelSettings<T extends Partial<Settings>>(settings:T, _defaultSource=DEFAULT_KERNEL_RELEASE_SOURCE):T & Pick<Settings, 'kernelPath'|'kernelArtifactPath'|'kernelReleaseSource'|'injectionMethod'> {
  return {...settings,kernelPath:null,kernelArtifactPath:null,kernelReleaseSource:DEFAULT_KERNEL_RELEASE_SOURCE,injectionMethod:'dwmapi'};
}
