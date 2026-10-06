import { createElement } from 'react';
import { act, create, type ReactTestRenderer } from 'react-test-renderer';
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';
import App, { claudeLoginRefreshMessageFor } from '../src/App';
import { backend } from '../src/services/backend';
import type { ClaudeLoginRefreshResult, QuotaData } from '../src/types/models';

vi.mock('../src/hooks/use_popover_window', () => ({ usePopoverWindow: () => false }));

function storage() {
  const values = new Map<string, string>([
    ['claude-quota-tab', 'claude'],
    ['claude-tray-enabled', 'false'], ['codex-tray-enabled', 'false'],
    ['cursor-tray-enabled', 'false'], ['grok-tray-enabled', 'false'],
    ['antigravity-tray-enabled', 'false'],
  ]);
  return {
    clear: () => values.clear(), getItem: (key: string) => values.get(key) ?? null,
    key: (index: number) => Array.from(values.keys())[index] ?? null,
    get length() { return values.size; }, removeItem: (key: string) => values.delete(key),
    setItem: (key: string, value: string) => values.set(key, value),
  };
}

function installBackend(quota: QuotaData): void {
  vi.spyOn(backend, 'getQuota').mockResolvedValue(quota);
  vi.spyOn(backend, 'refreshClaudeLogin').mockResolvedValue('refreshed');
  vi.spyOn(backend, 'getCodexInfo').mockResolvedValue({ connected: true });
  vi.spyOn(backend, 'getCodexRateLimits').mockResolvedValue({ connected: true });
  vi.spyOn(backend, 'getCodexResetCredits').mockResolvedValue({ connected: true, availableCount: 0, credits: [] });
  vi.spyOn(backend, 'getCodexWeeklyQuota').mockResolvedValue({});
  vi.spyOn(backend, 'getCursorInfo').mockResolvedValue({ connected: false });
  vi.spyOn(backend, 'getGrokInfo').mockResolvedValue({ connected: false, products: [] });
  vi.spyOn(backend, 'getAntigravityInfo').mockResolvedValue({ connected: false, status: 'pending' });
  vi.spyOn(backend, 'setDockVisibility').mockResolvedValue(undefined);
  vi.spyOn(backend, 'updateTrayIcon').mockResolvedValue(undefined);
}

async function flush(): Promise<void> {
  await Promise.resolve();
  await Promise.resolve();
  await Promise.resolve();
}

async function renderApp(): Promise<ReactTestRenderer> {
  let renderer!: ReactTestRenderer;
  await act(async () => { renderer = create(createElement(App)); await flush(); });
  return renderer;
}

async function clickRefresh(renderer: ReactTestRenderer): Promise<void> {
  await act(async () => {
    renderer.root.findByProps({ 'aria-label': 'Refresh current provider' }).props.onClick();
    await flush();
  });
}

const authError: QuotaData = {
  connected: false,
  error: 'Claude Code login expired. Click Refresh to renew it.',
};

beforeAll(() => { (globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true; });
beforeEach(() => {
  vi.useFakeTimers();
  (globalThis as Record<string, unknown>).localStorage = storage();
});
afterEach(() => {
  vi.clearAllTimers();
  vi.useRealTimers();
  vi.restoreAllMocks();
  delete (globalThis as Record<string, unknown>).localStorage;
});
afterAll(() => { delete (globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT; });

describe('Claude login recovery', () => {
  it('renews only a manual auth-error refresh, before fetching quota', async () => {
    installBackend(authError);
    vi.mocked(backend.refreshClaudeLogin).mockResolvedValue('unchanged');
    const renderer = await renderApp();
    await clickRefresh(renderer);
    expect(backend.refreshClaudeLogin).toHaveBeenCalledTimes(1);
    expect(backend.refreshClaudeLogin.mock.invocationCallOrder[0])
      .toBeLessThan(backend.getQuota.mock.invocationCallOrder.at(-1)!);
    expect(JSON.stringify(renderer.toJSON())).toContain(
      'Claude Code login is still expired. Open Claude Code and send a message, then click Refresh.',
    );
    await act(async () => renderer.unmount());
  });

  it.each([
    [{ connected: false, error: 'API error: 429 Too Many Requests' }, '429'],
    [{ connected: true }, 'no error'],
  ] satisfies Array<[QuotaData, string]>)('does not renew a manual %s refresh', async (quota) => {
    installBackend(quota);
    const renderer = await renderApp();
    await clickRefresh(renderer);
    expect(backend.refreshClaudeLogin).not.toHaveBeenCalled();
    await act(async () => renderer.unmount());
  });

  it('never renews from the automatic auth-error timer', async () => {
    installBackend(authError);
    const renderer = await renderApp();
    await act(async () => { await vi.advanceTimersByTimeAsync(60 * 60 * 1000); await flush(); });
    expect(backend.getQuota.mock.calls.length).toBeGreaterThanOrEqual(2);
    expect(backend.refreshClaudeLogin).not.toHaveBeenCalled();
    await act(async () => renderer.unmount());
  });

  it('clears a renewal message once a later quota fetch succeeds', async () => {
    const stillExpired = 'Claude Code login is still expired. Open Claude Code and send a message, then click Refresh.';
    installBackend(authError);
    vi.mocked(backend.refreshClaudeLogin).mockResolvedValue('unchanged');
    const renderer = await renderApp();
    await clickRefresh(renderer);
    expect(JSON.stringify(renderer.toJSON())).toContain(stillExpired);
    vi.mocked(backend.getQuota).mockResolvedValue({ connected: true });
    await act(async () => { await vi.advanceTimersByTimeAsync(60 * 60 * 1000); await flush(); });
    expect(JSON.stringify(renderer.toJSON())).not.toContain(stillExpired);
    // A later automatic auth error must show its own text, not the old renewal result.
    vi.mocked(backend.getQuota).mockResolvedValue(authError);
    await act(async () => { await vi.advanceTimersByTimeAsync(60 * 60 * 1000); await flush(); });
    const rendered = JSON.stringify(renderer.toJSON());
    expect(rendered).toContain(authError.error);
    expect(rendered).not.toContain(stillExpired);
    expect(backend.refreshClaudeLogin).toHaveBeenCalledTimes(1);
    await act(async () => renderer.unmount());
  });

  it('uses exact credential-safe result messages', () => {
    const expected: Record<Exclude<ClaudeLoginRefreshResult, 'refreshed'>, string> = {
      unchanged: 'Claude Code login is still expired. Open Claude Code and send a message, then click Refresh.',
      cliNotFound: 'Claude Code CLI not found. Sign in to Claude Code, then click Refresh.',
      failed: "Couldn't renew the Claude Code login. Try again in a minute.",
      throttled: "Couldn't renew the Claude Code login. Try again in a minute.",
    };
    expect(claudeLoginRefreshMessageFor('refreshed')).toBeNull();
    for (const [result, message] of Object.entries(expected) as Array<[keyof typeof expected, string]>) {
      expect(claudeLoginRefreshMessageFor(result)).toBe(message);
      expect(message).not.toMatch(/@|token=|\/(Users|home)\//);
    }
  });
});
