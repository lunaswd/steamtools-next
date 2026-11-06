import assert from 'node:assert/strict';
import { test } from 'node:test';
import { auditCommandAcl, auditProject } from './audit-command-acl.mjs';

const remote = ['plan_remote_kernel_install_command', 'install_remote_kernel_command'];
function fixture(commands = remote) {
  return {
    main: `tauri::generate_handler![${commands.join(', ')}]`,
    build: `AppManifest::new().commands(&[${commands.map(command => JSON.stringify(command)).join(', ')}])`,
    capability: { windows: ['main'], permissions: commands.map(command => `allow-${command.replaceAll('_', '-')}`) },
    api: commands.map(command => `invoke('${command}', {request})`).join('\n'),
  };
}

test('the real frontend, invoke handler, AppManifest and capability agree', () => {
  assert.deepEqual(auditProject().errors, []);
});
test('aligned remote commands are explicitly allowed', () => {
  assert.deepEqual(auditCommandAcl(fixture()).errors, []);
});
test('the original double rename omission catches the exact ACL symptom', () => {
  const old = fixture(['plan_kernel_install_command', 'install_kernel_command']);
  const current = fixture(); current.build = old.build; current.capability = old.capability;
  const errors = auditCommandAcl(current).errors;
  assert(errors.includes('Command install_remote_kernel_command not allowed by ACL: missing AppManifest declaration'));
  assert(errors.includes('Command install_remote_kernel_command not allowed by ACL: missing main capability permission'));
});
test('fixing only build.rs still rejects the omitted capability', () => {
  const current = fixture(); current.capability.permissions = [];
  assert(auditCommandAcl(current).errors.some(error => error.includes('missing main capability permission')));
});
test('fixing only the capability still rejects the omitted AppManifest', () => {
  const current = fixture(); current.build = 'AppManifest::new().commands(&[])';
  assert(auditCommandAcl(current).errors.some(error => error.includes('missing AppManifest declaration')));
});
test('stale local installation permissions are rejected', () => {
  const current = fixture(); current.capability.permissions.push('allow-install-kernel-command');
  assert(auditCommandAcl(current).errors.includes('Stale main capability permission: install_kernel_command'));
});
test('app permissions stay limited to the local main window', () => {
  const wrongWindow = fixture(); wrongWindow.capability.windows = ['other'];
  assert(auditCommandAcl(wrongWindow).errors.some(error => error.includes('main window')));
  const remoteOrigin = fixture(); remoteOrigin.capability.remote = { urls: ['https://example.invalid'] };
  assert(auditCommandAcl(remoteOrigin).errors.some(error => error.includes('remote origins')));
});
test('frontend command drift is rejected', () => {
  const current = fixture(); current.api += "\ninvoke('unknown_command')";
  assert(auditCommandAcl(current).errors.includes('Frontend invokes unregistered command: unknown_command'));
});
