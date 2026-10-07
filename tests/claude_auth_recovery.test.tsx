import { createElement } from 'react';
import { act, create, type ReactTestRenderer } from 'react-test-renderer';
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';
import App, { claudeLoginRefreshMessageFor } from '../src/App';
import { backend } from '../src/services/backend';
import {
  AUTO_REFRESH_INTERVAL_MS,
  AUTH_REFRESH_INTERVAL_MS,
  getClaudeRefreshIntervalMs,
  isClaudeAuthError,
  isClaudeSignedOutError,
} from '../src/services/app_state';
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
  vi.spyOn(backend, 'pingClaudeWindow').mockResolvedValue({ kind: 'sentUnconfirmed' });
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

function pingButton(renderer: ReactTestRenderer) {
  return renderer.root.find((node) => node.type === 'button'
    && node.props.className === 'action-btn ping-btn');
}

const authError: QuotaData = {
  connected: false,
  error: 'Claude Code login expired. Press Ping to renew it.',
};
const signedOutError: QuotaData = {
  connected: false,
  error: 'Claude Code is signed out. Run claude auth login in Terminal.',
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
    expect(backend.pingClaudeWindow).not.toHaveBeenCalled();
    expect(JSON.stringify(renderer.toJSON())).toContain(
      'Claude Code login is still expired. Press Ping to renew it.',
    );
    await act(async () => renderer.unmount());
  });

  it('uses Ping to confirm and renew an expired Claude Code login', async () => {
    installBackend(authError);
    const renderer = await renderApp();
    const button = pingButton(renderer);
    expect(button.props.disabled).toBeFalsy();
    expect(button.props.title).toBe('Renew Claude Code login — sends a ping (starts a 5-hour window)');
    await act(async () => { button.props.onClick(); await flush(); });
    expect(JSON.stringify(renderer.toJSON())).toContain(
      'Claude Code login expired. Send a ping to renew it? This starts a 5-hour window.',
    );
    await act(async () => {
      renderer.root.find((node) => node.type === 'button' && node.children.join('') === 'Send').props.onClick();
      await flush();
    });
    expect(backend.pingClaudeWindow).toHaveBeenCalledTimes(1);
    expect(backend.pingClaudeWindow).toHaveBeenCalledWith(true);
    await act(async () => renderer.unmount());
  });

  it('disables Ping and does not offer renewal for a signed-out Claude Code login', async () => {
    installBackend(signedOutError);
    const renderer = await renderApp();
    const button = pingButton(renderer);
    expect(button.props.disabled).toBe(true);
    expect(button.props.title).toBe('Sign in to Claude Code in Terminal first');
    await act(async () => { button.props.onClick(); await flush(); });
    expect(JSON.stringify(renderer.toJSON())).not.toContain('Send a ping to renew it?');
    expect(backend.pingClaudeWindow).not.toHaveBeenCalled();
    await act(async () => renderer.unmount());
  });

  it('clears an expired-login confirmation when Claude becomes signed out', async () => {
    installBackend(authError);
    const renderer = await renderApp();
    await act(async () => { pingButton(renderer).props.onClick(); await flush(); });
    expect(JSON.stringify(renderer.toJSON())).toContain('Send a ping to renew it?');

    vi.mocked(backend.getQuota).mockResolvedValue(signedOutError);
    await clickRefresh(renderer);
    expect(pingButton(renderer).props.disabled).toBe(true);
    expect(pingButton(renderer).props.title).toBe('Sign in to Claude Code in Terminal first');
    expect(JSON.stringify(renderer.toJSON())).not.toContain('Send a ping to renew it?');

    const actions = renderer.root.find((node) => typeof node.props.onPingConfirm === 'function');
    await act(async () => { actions.props.onPingConfirm(); await flush(); });
    expect(backend.pingClaudeWindow).not.toHaveBeenCalled();
    await act(async () => renderer.unmount());
  });

  it('keeps Ping disabled for non-auth Claude errors', async () => {
    installBackend({ connected: false, error: 'API error: 429 Too Many Requests' });
    const renderer = await renderApp();
    expect(pingButton(renderer).props.disabled).toBe(true);
    expect(backend.pingClaudeWindow).not.toHaveBeenCalled();
    await act(async () => renderer.unmount());
  });

  it('shows the Claude renewal failure guidance', async () => {
    installBackend(authError);
    vi.mocked(backend.pingClaudeWindow).mockResolvedValue({ kind: 'cliFailed', code: 'renewFailed' });
    const renderer = await renderApp();
    await act(async () => { pingButton(renderer).props.onClick(); await flush(); });
    await act(async () => {
      renderer.root.find((node) => node.type === 'button' && node.children.join('') === 'Send').props.onClick();
      await flush();
    });
    expect(JSON.stringify(renderer.toJSON())).toContain('Renew failed · sign in to Claude Code in Terminal');
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
    const stillExpired = 'Claude Code login is still expired. Press Ping to renew it.';
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
      unchanged: 'Claude Code login is still expired. Press Ping to renew it.',
      failed: "Couldn't read the Claude Code login. Try again.",
      signedOut: 'Claude Code is signed out. Run claude auth login in Terminal.',
    };
    expect(claudeLoginRefreshMessageFor('refreshed')).toBeNull();
    for (const [result, message] of Object.entries(expected) as Array<[keyof typeof expected, string]>) {
      expect(claudeLoginRefreshMessageFor(result)).toBe(message);
      expect(message).not.toMatch(/@|token=|\/(Users|home)\//);
    }
  });

  it('classifies signed-out errors before ordinary auth backoff', () => {
    const signedOut = signedOutError.error!;
    expect(isClaudeSignedOutError(signedOut)).toBe(true);
    expect(isClaudeAuthError(signedOut)).toBe(true);
    expect(getClaudeRefreshIntervalMs(signedOut)).toBe(AUTO_REFRESH_INTERVAL_MS);
    expect(isClaudeSignedOutError(authError.error!)).toBe(false);
    expect(getClaudeRefreshIntervalMs(authError.error!)).toBe(AUTH_REFRESH_INTERVAL_MS);
  });
});
