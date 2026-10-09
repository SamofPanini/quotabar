#!/usr/bin/env node
import { createHash } from 'node:crypto';
import { mkdir, readFile, rename, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const SOURCE_REPO = 'BerriAI/litellm';
const SOURCE_PATH = 'model_prices_and_context_window.json';
const SCRIPT_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');

function usage(message) {
  throw new Error(`${message}\nUsage: node scripts/update_pricing_snapshot.mjs --sha <commit> [--input <json> --license <file> --committed-at <iso>]`);
}

function parseArgs(argv) {
  const options = {};
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (!arg.startsWith('--')) usage(`Unexpected argument: ${arg}`);
    const value = argv[index + 1];
    if (!value || value.startsWith('--')) usage(`Missing value for ${arg}`);
    const key = arg.slice(2);
    if (!['sha', 'input', 'license', 'committed-at', 'output-dir'].includes(key)) {
      usage(`Unknown option: ${arg}`);
    }
    options[key] = value;
    index += 1;
  }
  if (!options.sha) usage('--sha is required; refusing to use an unspecified latest revision');
  const offline = options.input || options.license || options['committed-at'];
  if (offline && (!options.input || !options.license || !options['committed-at'])) {
    usage('Offline mode requires --input, --license, and --committed-at');
  }
  return { ...options, offline: Boolean(offline) };
}

function sha256(data) {
  return createHash('sha256').update(data).digest('hex');
}

function validateCommittedAt(value) {
  const timestamp = Date.parse(value);
  if (!Number.isFinite(timestamp) || !value.endsWith('Z')) {
    throw new Error('--committed-at must be an ISO 8601 UTC timestamp');
  }
  return new Date(timestamp).toISOString();
}

function validatePricing(value) {
  if (!value || typeof value !== 'object' || Array.isArray(value)) {
    throw new Error('Pricing input must have a JSON object at its top level');
  }
  const entries = Object.keys(value).length;
  if (entries === 0) throw new Error('Pricing input must contain at least one entry');
  const visit = (item, itemPath) => {
    if (!item || typeof item !== 'object') return;
    for (const [key, child] of Object.entries(item)) {
      const childPath = `${itemPath}.${key}`;
      if (key.endsWith('_cost_per_token') &&
          (typeof child !== 'number' || !Number.isFinite(child) || child < 0)) {
        throw new Error(`Invalid non-negative finite price at ${childPath}`);
      }
      visit(child, childPath);
    }
  };
  visit(value, 'pricing');
  return entries;
}

async function fetchOnline(sha) {
  const headers = { Accept: 'application/vnd.github+json', 'User-Agent': 'quotabar-pricing-snapshot' };
  const commitResponse = await fetch(`https://api.github.com/repos/${SOURCE_REPO}/commits/${sha}`, { headers });
  if (!commitResponse.ok) throw new Error(`GitHub commit request failed: ${commitResponse.status}`);
  const commit = await commitResponse.json();
  const committedAt = validateCommittedAt(commit?.commit?.committer?.date);
  const rawBase = `https://raw.githubusercontent.com/${SOURCE_REPO}/${sha}`;
  const [pricingResponse, licenseResponse] = await Promise.all([
    fetch(`${rawBase}/${SOURCE_PATH}`),
    fetch(`${rawBase}/LICENSE`),
  ]);
  if (!pricingResponse.ok) throw new Error(`GitHub pricing download failed: ${pricingResponse.status}`);
  if (!licenseResponse.ok) throw new Error(`GitHub license download failed: ${licenseResponse.status}`);
  return {
    pricingBytes: Buffer.from(await pricingResponse.arrayBuffer()),
    licenseBytes: Buffer.from(await licenseResponse.arrayBuffer()),
    committedAt,
  };
}

async function atomicWrite(destination, contents) {
  await mkdir(path.dirname(destination), { recursive: true });
  const temporary = `${destination}.${process.pid}.tmp`;
  await writeFile(temporary, contents);
  await rename(temporary, destination);
}

export async function updatePricingSnapshot(argv = process.argv.slice(2)) {
  const options = parseArgs(argv);
  let pricingBytes;
  let licenseBytes;
  let committedAt;
  if (options.offline) {
    [pricingBytes, licenseBytes] = await Promise.all([readFile(options.input), readFile(options.license)]);
    committedAt = validateCommittedAt(options['committed-at']);
  } else {
    ({ pricingBytes, licenseBytes, committedAt } = await fetchOnline(options.sha));
  }

  let pricing;
  try {
    pricing = JSON.parse(pricingBytes.toString('utf8'));
  } catch (error) {
    throw new Error(`Pricing input is not valid JSON: ${error.message}`);
  }
  const entries = validatePricing(pricing);
  const snapshot = Buffer.from(JSON.stringify(pricing));
  const outputRoot = path.resolve(options['output-dir'] ?? SCRIPT_ROOT);
  const pricingDir = path.join(outputRoot, 'src-tauri', 'resources', 'pricing');
  const source = {
    source_repo: SOURCE_REPO,
    source_path: SOURCE_PATH,
    commit_sha: options.sha,
    committed_at: committedAt,
    generated_at: new Date().toISOString(),
    upstream_sha256: sha256(pricingBytes),
    upstream_bytes: pricingBytes.length,
    snapshot_sha256: sha256(snapshot),
    snapshot_bytes: snapshot.length,
    entries,
    license: 'MIT',
  };

  // All parsing and validation precedes every write.
  await atomicWrite(path.join(pricingDir, 'litellm_model_prices.json'), snapshot);
  await atomicWrite(path.join(pricingDir, 'pricing-source.json'), `${JSON.stringify(source, null, 2)}\n`);
  await atomicWrite(path.join(pricingDir, 'LICENSE-litellm.txt'), licenseBytes);
  process.stdout.write(`entries=${entries} upstream_bytes=${pricingBytes.length} snapshot_bytes=${snapshot.length}\n`);
  process.stdout.write(`upstream_sha256=${source.upstream_sha256}\n`);
  process.stdout.write(`snapshot_sha256=${source.snapshot_sha256}\n`);
  return source;
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  updatePricingSnapshot().catch((error) => {
    process.stderr.write(`${error.message}\n`);
    process.exitCode = 1;
  });
}
