import { existsSync, readFileSync, statSync } from 'node:fs';
import { basename, join } from 'node:path';

const PRODUCT_BIN = 'quotabar';
const SMOKE_BIN = 'qbi_p1_r2_smoke';

function fail(message) {
  throw new Error(`product entrypoint guard: ${message}`);
}

function option(name) {
  const index = process.argv.indexOf(name);
  return index === -1 ? null : process.argv[index + 1] ?? fail(`missing value for ${name}`);
}

function tomlString(block, key) {
  const match = block.match(new RegExp(`^${key}\\s*=\\s*"([^"]+)"\\s*$`, 'm'));
  return match?.[1] ?? null;
}

function tomlStringArray(block, key) {
  const match = block.match(new RegExp(`^${key}\\s*=\\s*\\[([^\\]]*)\\]\\s*$`, 'm'));
  if (!match) return null;
  return [...match[1].matchAll(/"([^"]+)"/g)].map((entry) => entry[1]);
}

function verifyManifest(path) {
  const source = readFileSync(path, 'utf8');
  const packageBlock = source.split(/^\[lib\]/m, 1)[0];
  if (!/^autobins\s*=\s*false\s*$/m.test(packageBlock)) {
    fail('Cargo package must disable implicit bins with autobins = false');
  }
  if (tomlString(packageBlock, 'default-run') !== PRODUCT_BIN) {
    fail(`Cargo package default-run must be ${PRODUCT_BIN}`);
  }

  const featureBlock = source.match(/^\[features\]([\s\S]*?)(?=^\[[^\[]|^\[\[|(?![\s\S]))/m)?.[1] ?? '';
  if (!/^default\s*=\s*\[\]\s*$/m.test(featureBlock)) {
    fail('Cargo features must keep default = []');
  }
  if (!/^task-smoke\s*=\s*\[\]\s*$/m.test(featureBlock)) {
    fail('Cargo features must declare an opt-in task-smoke feature');
  }

  const bins = source.split(/^\[\[bin\]\]\s*$/m).slice(1).map((block) => ({
    name: tomlString(block, 'name'),
    path: tomlString(block, 'path'),
    requiredFeatures: tomlStringArray(block, 'required-features'),
  }));
  if (bins.length !== 2) {
    fail('Cargo manifest must declare exactly the product and task smoke bins');
  }
  const product = bins.find((bin) => bin.name === PRODUCT_BIN);
  if (!product || product.path !== 'src/main.rs' || product.requiredFeatures !== null) {
    fail('product bin must be quotabar at src/main.rs without required features');
  }
  const smoke = bins.find((bin) => bin.name === SMOKE_BIN);
  if (!smoke || smoke.path !== 'src/bin/qbi_p1_r2_smoke.rs'
      || JSON.stringify(smoke.requiredFeatures) !== JSON.stringify(['task-smoke'])) {
    fail('task smoke bin must require only the opt-in task-smoke feature');
  }
}

function plistExecutable(source) {
  const match = source.match(/<key>CFBundleExecutable<\/key>\s*<string>([^<]+)<\/string>/);
  return match?.[1] ?? null;
}

function verifyBundle(path) {
  const infoPlist = join(path, 'Contents', 'Info.plist');
  const executable = plistExecutable(readFileSync(infoPlist, 'utf8'));
  if (executable !== PRODUCT_BIN) {
    fail(`CFBundleExecutable must be ${PRODUCT_BIN}, received ${executable ?? 'missing'}`);
  }
  const executablePath = join(path, 'Contents', 'MacOS', executable);
  if (!existsSync(executablePath) || !statSync(executablePath).isFile()) {
    fail(`bundle product executable is missing: ${executablePath}`);
  }
  if (existsSync(join(path, 'Contents', 'MacOS', SMOKE_BIN))) {
    fail('task smoke executable leaked into the bundle');
  }
}

function verifyBuiltTarget(path) {
  if (!existsSync(path) || !statSync(path).isFile()) {
    fail(`built product target is missing: ${path}`);
  }
  if (!new RegExp(`^${PRODUCT_BIN}(?:\\.exe)?$`).test(basename(path))) {
    fail(`built target must be named ${PRODUCT_BIN} (or ${PRODUCT_BIN}.exe)`);
  }
}

const manifest = option('--manifest');
const bundle = option('--bundle');
const builtTarget = option('--built-target');
if (!manifest && !bundle && !builtTarget) {
  fail('provide --manifest, --bundle, or --built-target');
}
if (manifest) verifyManifest(manifest);
if (bundle) verifyBundle(bundle);
if (builtTarget) verifyBuiltTarget(builtTarget);
process.stdout.write('product entrypoint guard passed\n');
