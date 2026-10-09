import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const { test } = process.env.VITEST
  ? await import('vitest')
  : await import('node:test');

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const pricingDirectory = path.join(root, 'src-tauri', 'resources', 'pricing');

test('bundled LiteLLM pricing snapshot metadata matches its bytes', async () => {
  const [snapshot, rawSource] = await Promise.all([
    readFile(path.join(pricingDirectory, 'litellm_model_prices.json')),
    readFile(path.join(pricingDirectory, 'pricing-source.json'), 'utf8'),
  ]);
  const source = JSON.parse(rawSource);
  const pricing = JSON.parse(snapshot);
  assert.equal(source.snapshot_sha256, createHash('sha256').update(snapshot).digest('hex'));
  assert.equal(source.snapshot_bytes, snapshot.length);
  assert.equal(source.entries, Object.keys(pricing).length);
  for (const model of ['claude-haiku-5-5', 'gpt-5.6-luna', 'gpt-5.6-sol']) {
    assert.ok(Object.hasOwn(pricing, model), `missing ${model}`);
  }
});
