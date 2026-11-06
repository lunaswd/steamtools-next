import { readFileSync, readdirSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const projectRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const identifiers = text => [...text.matchAll(/\b([A-Za-z_][A-Za-z0-9_]*_command)\b/g)].map(match => match[1]);
const sorted = values => [...new Set(values)].sort();

/** Audit the exact application command boundary, not just backend function tests. */
export function auditCommandAcl({ main, build, capability, api }) {
  const handler = main.match(/tauri::generate_handler!\s*\[([\s\S]*?)\]/)?.[1];
  const appManifest = build.match(/AppManifest::new\(\)\.commands\(&\[([\s\S]*?)\]/)?.[1];
  const errors = [];
  if (!handler) errors.push('Missing Tauri invoke_handler registration list');
  if (!appManifest) errors.push('Missing Tauri AppManifest command allowlist');
  const registered = sorted(identifiers(handler || ''));
  const declared = sorted(identifiers(appManifest || ''));
  const permitted = sorted((capability.permissions || [])
    .filter(value => typeof value === 'string' && value.startsWith('allow-') && !value.includes(':'))
    .map(value => value.slice('allow-'.length).replaceAll('-', '_')));
  if (!(capability.windows || []).includes('main')) errors.push('Main capability does not select the main window');
  if (capability.local === false) errors.push('Main capability disables local IPC');
  if (capability.remote) errors.push('Main capability must not grant app commands to remote origins');
  for (const command of registered) {
    if (!declared.includes(command)) errors.push(`Command ${command} not allowed by ACL: missing AppManifest declaration`);
    if (!permitted.includes(command)) errors.push(`Command ${command} not allowed by ACL: missing main capability permission`);
  }
  for (const command of declared) {
    if (!registered.includes(command)) errors.push(`Stale AppManifest declaration: ${command}`);
  }
  for (const command of permitted) {
    if (!registered.includes(command)) errors.push(`Stale main capability permission: ${command}`);
  }
  const invoked = sorted([...api.matchAll(/invoke(?:<[^>]*>)?\(\s*['"]([A-Za-z_][A-Za-z0-9_]*_command)['"]/g)].map(match => match[1]));
  for (const command of invoked) {
    if (!registered.includes(command)) errors.push(`Frontend invokes unregistered command: ${command}`);
  }
  return { registered, declared, permitted, invoked, errors };
}

export function auditProject(root = projectRoot) {
  const frontendFiles = readdirSync(resolve(root, 'src'), { recursive: true })
    .filter(name => name.endsWith('.ts') && !name.endsWith('.test.ts'));
  return auditCommandAcl({
    main: readFileSync(resolve(root, 'src-tauri/src/main.rs'), 'utf8'),
    build: readFileSync(resolve(root, 'src-tauri/build.rs'), 'utf8'),
    capability: JSON.parse(readFileSync(resolve(root, 'src-tauri/capabilities/main.json'), 'utf8')),
    api: frontendFiles.map(name => readFileSync(resolve(root, 'src', name), 'utf8')).join('\n'),
  });
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const result = auditProject(process.argv[2] ? resolve(process.argv[2]) : projectRoot);
  if (result.errors.length) {
    console.error(result.errors.join('\n'));
    process.exitCode = 1;
  } else {
    console.log(`Tauri command ACL audit passed: ${result.registered.length} registered commands; local main window only.`);
  }
}
