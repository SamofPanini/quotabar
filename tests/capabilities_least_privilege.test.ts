import { readdirSync, readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';

const capabilityDirectory = new URL('../src-tauri/capabilities/', import.meta.url);
const sourceDirectory = new URL('../src/', import.meta.url);
const allowedOpenerPermissions = new Set([
  'opener:deny-open-url',
  'opener:deny-open-path',
  'opener:deny-reveal-item-in-dir',
]);
const defaultPermissions = new Set([
  'core:default',
  'core:webview:allow-set-webview-zoom',
  'notification:default',
  'autostart:default',
  ...allowedOpenerPermissions,
]);

function filesRecursively(directory: URL, extensions: Set<string>): URL[] {
  return readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    const file = new URL(entry.name, directory);
    if (entry.isDirectory()) return filesRecursively(new URL(`${entry.name}/`, directory), extensions);
    return extensions.has(entry.name.slice(entry.name.lastIndexOf('.'))) ? [file] : [];
  });
}

function permissionIdentifier(permission: unknown): string {
  if (typeof permission === 'string') return permission;
  if (
    typeof permission === 'object'
    && permission !== null
    && 'identifier' in permission
    && typeof permission.identifier === 'string'
  ) {
    return permission.identifier;
  }
  throw new Error(`Unsupported capability permission: ${JSON.stringify(permission)}`);
}

describe('opener least privilege capabilities', () => {
  it('allows only explicit opener denies in every capability file', () => {
    const capabilityFiles = filesRecursively(capabilityDirectory, new Set(['.json']));
    expect(capabilityFiles.length).toBeGreaterThan(0);

    for (const file of capabilityFiles) {
      const capability = JSON.parse(readFileSync(file, 'utf8')) as { permissions?: unknown[] };
      expect(Array.isArray(capability.permissions), `${file.pathname} has permissions`).toBe(true);
      for (const permission of capability.permissions ?? []) {
        const identifier = permissionIdentifier(permission);
        if (identifier.startsWith('opener:')) {
          expect(allowedOpenerPermissions, `${file.pathname}: ${identifier}`).toContain(identifier);
        }
      }
    }
  });

  it('keeps every capability in JSON files and none inline in tauri.conf.json', () => {
    // Tauri also loads .toml capabilities and inline app.security.capabilities;
    // either would bypass the JSON scan above.
    const entries = readdirSync(capabilityDirectory, { withFileTypes: true });
    const nonJson = entries.filter((entry) => !entry.isFile() || !entry.name.endsWith('.json'));
    expect(nonJson.map((entry) => entry.name)).toEqual([]);

    const config = JSON.parse(
      readFileSync(new URL('../src-tauri/tauri.conf.json', import.meta.url), 'utf8'),
    ) as { app?: { security?: { capabilities?: unknown[] } } };
    expect(config.app?.security?.capabilities ?? []).toEqual([]);
  });

  it('keeps the default capability permission set exact', () => {
    const capability = JSON.parse(
      readFileSync(new URL('../src-tauri/capabilities/default.json', import.meta.url), 'utf8'),
    ) as { permissions: unknown[] };
    expect(new Set(capability.permissions.map(permissionIdentifier))).toEqual(defaultPermissions);
    expect(capability.permissions).toHaveLength(defaultPermissions.size);
  });

  it('keeps opener calls out of frontend source', () => {
    const sourceFiles = [
      ...filesRecursively(sourceDirectory, new Set(['.ts', '.tsx', '.js', '.jsx', '.html'])),
      new URL('../index.html', import.meta.url),
    ];
    expect(sourceFiles.length).toBeGreaterThan(1);
    for (const file of sourceFiles) {
      const source = readFileSync(file, 'utf8');
      expect(source, file.pathname).not.toContain('@tauri-apps/plugin-opener');
      expect(source, file.pathname).not.toContain('plugin:opener');
    }
  });

  it('disables JavaScript link interception by the opener plugin', () => {
    const source = readFileSync(new URL('../src-tauri/src/lib.rs', import.meta.url), 'utf8');
    expect(source).toContain('open_js_links_on_click(false)');
    expect(source).not.toContain('tauri_plugin_opener::init()');
  });
});
