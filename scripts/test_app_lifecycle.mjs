import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { existsSync, mkdtempSync, mkdirSync, readFileSync, realpathSync, readdirSync, rmSync, symlinkSync, writeFileSync, chmodSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

const root = dirname(dirname(fileURLToPath(import.meta.url)));
const scripts = Object.fromEntries(['install_app.sh', 'stop_app.sh', 'run_app.sh', 'reinstall_and_run.sh'].map((name) => [name, join(root, 'scripts', name)]));

function makeFixture() {
  const base = realpathSync(mkdtempSync(join(tmpdir(), 'quotabar-lifecycle-')));
  const applications = join(base, 'Applications');
  const source = join(base, 'src-tauri', 'target', 'release', 'bundle', 'macos', 'QuotaBar.app');
  const bin = join(base, 'bin');
  mkdirSync(applications, { recursive: true });
  mkdirSync(bin);
  makeBundle(source, 'new');
  const events = join(base, 'events');
  const stub = `#!/usr/bin/env bash
set -eu
name="$(basename "$0")"
echo "$name:$*" >> "$EVENTS"
case "$name" in
  pkill) exit "\${PKILL_STATUS:-0}" ;;
  pgrep)
    count_file="$EVENTS.pgrep-count"; count=0; [[ -f "$count_file" ]] && count=$(cat "$count_file")
    count=$((count + 1)); echo "$count" > "$count_file"
    IFS=, read -r -a sequence <<< "\${PGREP_SEQUENCE:-1}"
    index=$((count - 1)); status="\${sequence[$index]:-1}"; exit "$status" ;;
  sleep) exit 0 ;;
  open) exit "\${OPEN_STATUS:-0}" ;;
  ditto)
    /bin/cp -R "$1" "$2"
    exit "\${DITTO_STATUS:-0}" ;;
  mv)
    count_file="$EVENTS.mv-count"; count=0; [[ -f "$count_file" ]] && count=$(cat "$count_file")
    count=$((count + 1)); echo "$count" > "$count_file"
    fail_calls=",\${FAIL_MV_CALLS:-\${FAIL_MV_CALL:-0}},"
    [[ "$fail_calls" != *",$count,"* ]] || exit "\${MV_STATUS:-75}"
    if [[ "$count" == "\${MOVE_THEN_FAIL_CALL:-0}" ]]; then /bin/mv "$1" "$2"; exit "\${MV_STATUS:-75}"; fi
    /bin/mv "$1" "$2" ;;
esac
`;
  for (const name of ['pkill', 'pgrep', 'sleep', 'open', 'ditto', 'mv']) {
    const path = join(bin, name);
    writeFileSync(path, stub);
    chmodSync(path, 0o755);
  }
  return { base, applications, source, destination: join(applications, 'QuotaBar.app'), bin, events };
}

function makeBundle(path, marker) {
  mkdirSync(join(path, 'Contents', 'MacOS'), { recursive: true });
  writeFileSync(join(path, 'Contents', 'MacOS', 'quotabar'), '#!/bin/sh\n');
  chmodSync(join(path, 'Contents', 'MacOS', 'quotabar'), 0o755);
  writeFileSync(join(path, 'marker'), marker);
}

function run(fixture, script, extra = {}) {
  const env = {
    ...process.env,
    PATH: `${fixture.bin}:/usr/bin:/bin`,
    EVENTS: fixture.events,
    QUOTABAR_APP_DST: fixture.destination,
    QUOTABAR_APP_SRC: fixture.source,
    ...extra,
  };
  for (const name of ['QUOTABAR_APP_DST', 'QUOTABAR_APP_SRC']) {
    if (extra[name] === null) delete env[name];
  }
  return spawnSync('bash', [scripts[script]], { cwd: fixture.base, encoding: 'utf8', env });
}

function events(fixture) {
  return existsSync(fixture.events) ? readFileSync(fixture.events, 'utf8').trim().split('\n').filter(Boolean) : [];
}

function stages(fixture) {
  return readdirSync(fixture.applications).filter((name) => name.startsWith('.quotabar-install.'));
}

function withFixture(fn) {
  const fixture = makeFixture();
  try { fn(fixture); } finally { rmSync(fixture.base, { recursive: true, force: true }); }
}

test('app lifecycle scripts', { skip: process.platform !== 'darwin' }, async (t) => {
  await t.test('run opens only the validated destination and propagates open failures', () => withFixture((f) => {
    makeBundle(f.destination, 'old');
    assert.equal(run(f, 'run_app.sh').status, 0);
    assert.deepEqual(events(f), [`open:${f.destination}`]);
    rmSync(f.destination, { recursive: true });
    assert.equal(run(f, 'run_app.sh').status, 1);
    assert.match(run(f, 'run_app.sh').stderr, /not installed/);
    assert.deepEqual(events(f), [`open:${f.destination}`]);
    assert.equal(run(f, 'run_app.sh', { OPEN_STATUS: '23' }).status, 1, 'missing app guards open');
    makeBundle(f.destination, 'old');
    assert.equal(run(f, 'run_app.sh', { OPEN_STATUS: '23' }).status, 23);
  }));

  await t.test('stop distinguishes absent, errors, polling success, and timeout', () => withFixture((f) => {
    let result = run(f, 'stop_app.sh', { PKILL_STATUS: '1' });
    assert.equal(result.status, 0); assert.match(result.stdout, /not running/);
    result = run(f, 'stop_app.sh', { PKILL_STATUS: '7' });
    assert.equal(result.status, 7); assert.doesNotMatch(result.stdout, /Stopped/);
    assert.deepEqual(stages(f), []);
    result = run(f, 'stop_app.sh', { PGREP_SEQUENCE: '0,0,1' });
    assert.equal(result.status, 0);
    assert.equal(events(f).filter((line) => line.startsWith('sleep:')).length, 2);
    rmSync(`${f.events}.pgrep-count`, { force: true });
    result = run(f, 'stop_app.sh', { PGREP_SEQUENCE: '3' });
    assert.equal(result.status, 3);
    rmSync(`${f.events}.pgrep-count`, { force: true });
    const sleepsBeforeTimeout = events(f).filter((line) => line.startsWith('sleep:')).length;
    result = run(f, 'stop_app.sh', { PGREP_SEQUENCE: Array(51).fill('0').join(',') });
    assert.equal(result.status, 1); assert.match(result.stderr, /Timed out/);
    assert.equal(events(f).filter((line) => line.startsWith('sleep:')).length - sleepsBeforeTimeout, 50);
    assert.deepEqual(stages(f), []);
  }));

  await t.test('stop pattern is anchored and escapes destination ERE metacharacters', () => withFixture((f) => {
    const dotted = join(f.applications, 'QuotaBar.app');
    const result = run(f, 'stop_app.sh', { PKILL_STATUS: '1', QUOTABAR_APP_DST: dotted });
    assert.equal(result.status, 0);
    const pattern = events(f).find((line) => line.startsWith('pkill:')).replace(/^pkill:-TERM -f /, '');
    const target = `${dotted}/Contents/MacOS/quotabar`;
    const matches = (value) => spawnSync('grep', ['-E', '-q', pattern], { input: `${value}\n` }).status === 0;
    assert.equal(matches(target), true);
    assert.equal(matches(`${target} --flag`), true);
    assert.equal(matches(`${target}-helper`), false);
    assert.equal(matches(`echo ${target}`), false);
    assert.equal(matches(`${f.base}/other/src-tauri/target/release/quotabar`), false);
    assert.equal(matches(target.replace('QuotaBar.app', 'QuotaBarXapp')), false);
  }));

  await t.test('install stages before stop, replaces atomically, and rolls back failures', () => withFixture((f) => {
    makeBundle(f.destination, 'old');
    writeFileSync(join(f.destination, 'obsolete'), 'old-only');
    let result = run(f, 'install_app.sh', { DITTO_STATUS: '9' });
    assert.equal(result.status, 9); assert.equal(readFileSync(join(f.destination, 'marker'), 'utf8'), 'old');
    assert.equal(events(f).filter((line) => line.startsWith('pkill:')).length, 0);
    assert.deepEqual(readdirSync(f.applications), ['QuotaBar.app']);
    result = run(f, 'install_app.sh', { PKILL_STATUS: '7' });
    assert.equal(result.status, 7); assert.equal(readFileSync(join(f.destination, 'marker'), 'utf8'), 'old');
    assert.deepEqual(stages(f), []);
    result = run(f, 'install_app.sh', { PGREP_SEQUENCE: Array(51).fill('0').join(',') });
    assert.equal(result.status, 1); assert.equal(readFileSync(join(f.destination, 'marker'), 'utf8'), 'old');
    assert.deepEqual(stages(f), []);
    result = run(f, 'install_app.sh');
    assert.equal(result.status, 0); assert.equal(readFileSync(join(f.destination, 'marker'), 'utf8'), 'new');
    assert.equal(existsSync(join(f.destination, 'obsolete')), false);
    assert.match(events(f).join('\n'), /ditto:.*\npkill:.*\npgrep:.*\nmv:.*\nmv:/);
  }));

  await t.test('install supports first install and preserves old bundle on replacement failure', () => withFixture((f) => {
    let result = run(f, 'install_app.sh');
    assert.equal(result.status, 0); assert.equal(readFileSync(join(f.destination, 'marker'), 'utf8'), 'new');
    rmSync(f.destination, { recursive: true }); makeBundle(f.destination, 'old');
    rmSync(`${f.events}.mv-count`, { force: true });
    result = run(f, 'install_app.sh', { FAIL_MV_CALL: '2', MV_STATUS: '75' });
    assert.equal(result.status, 75); assert.equal(readFileSync(join(f.destination, 'marker'), 'utf8'), 'old');
    rmSync(`${f.events}.mv-count`, { force: true });
    result = run(f, 'install_app.sh', { FAIL_MV_CALLS: '2,3', MV_STATUS: '76' });
    assert.equal(result.status, 76); assert.match(result.stderr, /previous\.app/);
    const retained = result.stderr.match(/at (.*previous\.app)/)[1];
    assert.equal(existsSync(retained), true);
    assert.equal(readFileSync(join(retained, 'marker'), 'utf8'), 'old');
  }));

  await t.test('missing source does no work and reinstall opens only after install', () => withFixture((f) => {
    makeBundle(f.destination, 'old');
    const missing = join(f.base, 'missing', 'QuotaBar.app');
    let result = run(f, 'install_app.sh', { QUOTABAR_APP_SRC: missing });
    assert.equal(result.status, 1); assert.match(result.stderr, /App bundle not found/);
    assert.deepEqual(events(f), []); assert.equal(readFileSync(join(f.destination, 'marker'), 'utf8'), 'old');
    result = run(f, 'install_app.sh', { QUOTABAR_APP_SRC: 'src-tauri/target/release/bundle/macos/QuotaBar.app' });
    assert.equal(result.status, 2); assert.deepEqual(events(f), []);
    result = run(f, 'reinstall_and_run.sh');
    assert.equal(result.status, 0);
    assert.match(events(f).at(0), /^ditto:/);
    assert.equal(events(f).filter((line) => line.startsWith('pkill:')).length, 1);
    assert.match(events(f).join('\n'), /ditto:.*\npkill:.*\npgrep:.*\nmv:.*\nmv:.*\nopen:/);
  }));

  await t.test('every script requires and rejects unsafe destinations before invoking a stub', () => withFixture((f) => {
    mkdirSync(join(f.base, 'RelativeApps'));
    const cases = [
      ['RelativeApps/QuotaBar.app', 'relative path'],
      [join(f.applications, 'Wrong.app'), 'wrong basename'],
      [join(f.base, 'missing-parent', 'QuotaBar.app'), 'missing parent'],
    ];
    makeBundle(join(f.base, 'link-target', 'QuotaBar.app'), 'link-content');
    const link = join(f.applications, 'QuotaBar.app');
    symlinkSync(join(f.base, 'link-target', 'QuotaBar.app'), link);
    cases.push([link, 'symbolic link']);
    cases.push([`${link}/`, 'symbolic link with trailing slash']);
    for (const [destination, label] of cases) {
      for (const script of Object.keys(scripts)) {
        const result = run(f, script, { QUOTABAR_APP_DST: destination });
        assert.equal(result.status, 2, `${label}: ${script}`);
      }
    }
    assert.deepEqual(events(f), []);
    assert.equal(readFileSync(join(f.base, 'link-target', 'QuotaBar.app', 'marker'), 'utf8'), 'link-content');
  }));

  await t.test('unset or empty destination defaults to /Applications/QuotaBar.app (sourced only, no actions)', () => withFixture((f) => {
    for (const destination of [null, '']) {
      const env = { ...process.env, PATH: `${f.bin}:/usr/bin:/bin`, EVENTS: f.events };
      delete env.QUOTABAR_APP_DST;
      if (destination === '') env.QUOTABAR_APP_DST = '';
      const result = spawnSync('bash', ['-c', 'source "$1"; printf "%s\\n%s" "$APP_DST" "$APP_PROCESS_PATTERN"', 'probe', join(root, 'scripts', 'app_paths.sh')], { cwd: f.base, encoding: 'utf8', env });
      assert.equal(result.status, 0, result.stderr);
      assert.deepEqual(result.stdout.split('\n'), ['/Applications/QuotaBar.app', '^/Applications/QuotaBar\\.app/Contents/MacOS/quotabar([[:space:]].*)?$']);
    }
    assert.deepEqual(events(f), []);
  }));

  await t.test('an interrupted replacement keeps the moved-aside old bundle', () => withFixture((f) => {
    makeBundle(f.destination, 'old');
    const result = run(f, 'install_app.sh', { MOVE_THEN_FAIL_CALL: '1', MV_STATUS: '77' });
    assert.equal(result.status, 77);
    const retained = stages(f).map((stage) => join(f.applications, stage, 'previous.app')).filter((path) => existsSync(path));
    assert.equal(retained.length, 1, result.stderr);
    assert.equal(readFileSync(join(retained[0], 'marker'), 'utf8'), 'old');
  }));

  await t.test('reinstall does not open after replacement or restoration failure', () => withFixture((f) => {
    makeBundle(f.destination, 'old');
    let result = run(f, 'reinstall_and_run.sh', { FAIL_MV_CALL: '2', MV_STATUS: '75' });
    assert.equal(result.status, 75);
    assert.equal(events(f).some((line) => line.startsWith('open:')), false);
    rmSync(`${f.events}.mv-count`, { force: true });
    result = run(f, 'reinstall_and_run.sh', { FAIL_MV_CALLS: '2,3', MV_STATUS: '76' });
    assert.equal(result.status, 76);
    assert.equal(events(f).some((line) => line.startsWith('open:')), false);
    assert.match(result.stderr, /previous\.app/);
  }));
});
