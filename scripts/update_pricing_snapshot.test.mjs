import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdtemp, readFile, writeFile, access } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { updatePricingSnapshot } from './update_pricing_snapshot.mjs';

const { test } = process.env.VITEST
  ? await import('vitest')
  : await import('node:test');

const sha256 = (value) => createHash('sha256').update(value).digest('hex');

async function fixtureDirectory() {
  const directory = await mkdtemp(path.join(os.tmpdir(), 'quotabar-pricing-'));
  const input = path.join(directory, 'input.json');
  const license = path.join(directory, 'LICENSE');
  await writeFile(input, '{\n  "sample": { "input_cost_per_token": 0.1 }\n}\n');
  await writeFile(license, 'MIT fixture license\n');
  return { directory, input, license };
}

test('offline snapshot records hashes and compact JSON', async () => {
  const { directory, input, license } = await fixtureDirectory();
  const source = await updatePricingSnapshot([
    '--input', input, '--license', license, '--sha', 'fixture-sha',
    '--committed-at', '2026-10-09T11:56:18Z', '--output-dir', directory,
  ]);
  const pricingDir = path.join(directory, 'src-tauri', 'resources', 'pricing');
  const snapshot = await readFile(path.join(pricingDir, 'litellm_model_prices.json'));
  const metadata = JSON.parse(await readFile(path.join(pricingDir, 'pricing-source.json'), 'utf8'));
  assert.equal(snapshot.toString(), '{"sample":{"input_cost_per_token":0.1}}');
  assert.equal(metadata.snapshot_sha256, sha256(snapshot));
  assert.equal(metadata.snapshot_bytes, snapshot.length);
  assert.equal(metadata.upstream_sha256, sha256(await readFile(input)));
  assert.equal(metadata.entries, 1);
  assert.equal(source.license, 'MIT');
});

test('bad offline inputs refuse to write output', async () => {
  const { directory, input, license } = await fixtureDirectory();
  await writeFile(input, '[]');
  await assert.rejects(() => updatePricingSnapshot([
    '--input', input, '--license', license, '--sha', 'fixture-sha',
    '--committed-at', '2026-10-09T11:56:18Z', '--output-dir', directory,
  ]), /top level/);
  await assert.rejects(access(path.join(directory, 'src-tauri', 'resources', 'pricing', 'litellm_model_prices.json')));
  await writeFile(input, '{"sample":{"output_cost_per_token":-1}}');
  await assert.rejects(() => updatePricingSnapshot([
    '--input', input, '--license', license, '--sha', 'fixture-sha',
    '--committed-at', '2026-10-09T11:56:18Z', '--output-dir', directory,
  ]), /non-negative finite/);
});
