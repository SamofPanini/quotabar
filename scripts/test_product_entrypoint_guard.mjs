import { chmodSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';
import { join } from 'node:path';
import { spawnSync } from 'node:child_process';

const root = mkdtempSync(join(tmpdir(), 'quotabar-entrypoint-guard-'));
const guard = fileURLToPath(new URL('./check_product_entrypoint.mjs', import.meta.url));

function write(path, contents) {
  writeFileSync(path, contents, 'utf8');
}

function run(...args) {
  return spawnSync(process.execPath, [guard, ...args], { encoding: 'utf8' });
}

function expectPass(...args) {
  const result = run(...args);
  if (result.status !== 0) throw new Error(result.stderr || result.stdout);
}

function expectFail(...args) {
  const result = run(...args);
  if (result.status === 0) throw new Error(`expected guard failure for ${args.join(' ')}`);
}

try {
  const manifest = join(root, 'Cargo.toml');
  write(manifest, `[package]\nname = "quotabar"\nautobins = false\ndefault-run = "quotabar"\n\n[features]\ndefault = []\ntask-smoke = []\n\n[[bin]]\nname = "quotabar"\npath = "src/main.rs"\n\n[[bin]]\nname = "qbi_p1_r2_smoke"\npath = "task-bin/qbi_p1_r2_smoke.rs"\nrequired-features = ["task-smoke"]\n`);
  const bundle = join(root, 'QuotaBar.app');
  const contents = join(bundle, 'Contents');
  const macos = join(contents, 'MacOS');
  await import('node:fs/promises').then(({ mkdir }) => mkdir(macos, { recursive: true }));
  write(join(contents, 'Info.plist'), '<?xml version="1.0"?><plist><dict><key>CFBundleExecutable</key><string>quotabar</string></dict></plist>');
  write(join(macos, 'quotabar'), 'fixture');
  chmodSync(join(macos, 'quotabar'), 0o755);

  expectPass('--manifest', manifest);
  write(manifest, `[package]\nname = "quotabar"\nautobins = false\ndefault-run = "quotabar"\n\n[features]\ndefault = []\ntask-smoke = []\n\n[[bin]]\nname = "quotabar"\npath = "src/main.rs"\n\n[[bin]]\nname = "qbi_p1_r2_smoke"\npath = "task-bin/qbi_p1_r2_smoke.rs"\n`);
  expectFail('--manifest', manifest);
  write(manifest, `[package]\nname = "quotabar"\nautobins = false\ndefault-run = "quotabar"\n\n[features]\ndefault = []\ntask-smoke = []\n\n[[bin]]\nname = "quotabar"\npath = "src/main.rs"\n\n[[bin]]\nname = "qbi_p1_r2_smoke"\npath = "src/bin/qbi_p1_r2_smoke.rs"\nrequired-features = ["task-smoke"]\n`);
  expectFail('--manifest', manifest);
  write(manifest, `[package]\nname = "quotabar"\nautobins = false\ndefault-run = "quotabar"\n\n[features]\ndefault = []\ntask-smoke = []\n\n[[bin]]\nname = "quotabar"\npath = "src/main.rs"\n\n[[bin]]\nname = "qbi_p1_r2_smoke"\npath = "task-bin/qbi_p1_r2_smoke.rs"\nrequired-features = ["task-smoke"]\n`);
  expectPass('--bundle', bundle);
  write(join(contents, 'Info.plist'), '<?xml version="1.0"?><plist><dict><key>CFBundleExecutable</key><string>qbi_p1_r2_smoke</string></dict></plist>');
  expectFail('--bundle', bundle);
  write(join(contents, 'Info.plist'), '<?xml version="1.0"?><plist><dict><key>CFBundleExecutable</key><string>quotabar</string></dict></plist>');
  rmSync(join(macos, 'quotabar'));
  expectFail('--bundle', bundle);
  write(manifest, '[package]\nname = "quotabar"\n');
  expectFail('--manifest', manifest);
  process.stdout.write('product entrypoint guard fixtures passed\n');
} finally {
  rmSync(root, { recursive: true, force: true });
}
