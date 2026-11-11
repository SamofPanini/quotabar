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
  for (const model of ['claude-fable-5-2', 'claude-haiku-5-5', 'gpt-5.6-luna', 'gpt-5.6-sol', 'gpt-6.1-terra']) {
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

test('bundled claude-fable-5-2 prices match the published standard rates', async () => {
  const pricing = JSON.parse(await readFile(path.join(pricingDirectory, 'litellm_model_prices.json'), 'utf8'));
  const fable = pricing['claude-fable-5-2'];
  const rates = {
    input_cost_per_token: 8e-6,
    output_cost_per_token: 4e-5,
    cache_read_input_token_cost: 2e-7,
    cache_creation_input_token_cost: 1e-5,
    cache_creation_input_token_cost_above_1hr: 1.6e-5,
  };
  for (const [field, expected] of Object.entries(rates)) {
    assert.equal(fable[field], expected, field);
  }
  // 10K input, 2K output, 100K cache read, 3K 5-minute cache write, 2K 1-hour cache write.
  const cost = 10_000 * fable.input_cost_per_token
    + 2_000 * fable.output_cost_per_token
    + 100_000 * fable.cache_read_input_token_cost
    + 3_000 * fable.cache_creation_input_token_cost
    + 2_000 * fable.cache_creation_input_token_cost_above_1hr;
  assert.ok(Math.abs(cost - 0.242) < 1e-9, `fixture cost ${cost}`);
});

test('bundled claude-haiku-5-5 keeps its above-100k rates for ccstats 0.11.1', async () => {
  const pricing = JSON.parse(await readFile(path.join(pricingDirectory, 'litellm_model_prices.json'), 'utf8'));
  const haiku = pricing['claude-haiku-5-5'];
  assert.ok(haiku.input_cost_per_token_above_100k_tokens > haiku.input_cost_per_token);
  assert.ok(haiku.output_cost_per_token_above_100k_tokens > haiku.output_cost_per_token);
});
