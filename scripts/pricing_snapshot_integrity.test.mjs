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
  for (const model of ['claude-haiku-5-5', 'gpt-5.6-luna', 'gpt-5.6-sol', 'gpt-6.1-terra']) {
    assert.ok(Object.hasOwn(pricing, model), `missing ${model}`);
  }
});

test('bundled gpt-6.1-terra prices match the expected rates', async () => {
  const pricing = JSON.parse(await readFile(path.join(pricingDirectory, 'litellm_model_prices.json'), 'utf8'));
  const terra = pricing['gpt-6.1-terra'];
  for (const [field, expected] of Object.entries({
    input_cost_per_token: 1e-6,
    cache_read_input_token_cost: 5e-8,
    output_cost_per_token: 5e-6,
    input_cost_per_token_above_272k_tokens: 2e-6,
    cache_read_input_token_cost_above_272k_tokens: 1e-7,
    output_cost_per_token_above_272k_tokens: 7.5e-6,
  })) {
    assert.equal(terra[field], expected, field);
  }
});
