import { createElement } from 'react';
import { act, create, type ReactTestRenderer } from 'react-test-renderer';
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';
import App from '../src/App';
import { backend } from '../src/services/backend';
import type { CodexProfilesResponse, PingOutcome } from '../src/types/models';

vi.mock('../src/hooks/use_popover_window', () => ({
  usePopoverWindow: () => false,
}));

function memoryStorage(initial: Record<string, string>) {
  const values = new Map(Object.entries(initial));
  return {
    clear: () => values.clear(),
    getItem: (key: string) => values.get(key) ?? null,
    key: (index: number) => Array.from(values.keys())[index] ?? null,
    get length() { return values.size; },
    removeItem: (key: string) => { values.delete(key); },
    setItem: (key: string, value: string) => { values.set(key, value); },
  };
}

interface Deferred<T> {
  promise: Promise<T>;
  resolve(value: T): void;
}

function deferred<T>(): Deferred<T> {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((next) => { resolve = next; });
  return { promise, resolve };
}

async function flush(): Promise<void> {
  await Promise.resolve();
  await Promise.resolve();
  await Promise.resolve();
}

async function renderApp(): Promise<ReactTestRenderer> {
  let renderer!: ReactTestRenderer;
  await act(async () => {
    renderer = create(createElement(App));
    await flush();
  });
  return renderer;
}

function clickProvider(renderer: ReactTestRenderer, provider: string): Promise<void> {
  return act(async () => {
    renderer.root.findByProps({ 'data-provider': provider }).props.onClick();
    await flush();
  });
}

function pingButton(renderer: ReactTestRenderer) {
  return renderer.root.find((node) => (
    node.type === 'button'
    && typeof node.props['aria-label'] === 'string'
    && node.props['aria-label'].startsWith('Ping ')
  ));
}

function accountTab(renderer: ReactTestRenderer, label: string) {
  return renderer.root.find((node) => (
    node.type === 'button'
    && node.props.role === 'tab'
    && node.children.join('') === label
  ));
}

function text(renderer: ReactTestRenderer): string {
  return JSON.stringify(renderer.toJSON());
}

function installBackend(): void {
  const resetsAt = Math.floor(Date.now() / 1000) + 5 * 60 * 60;
  vi.spyOn(backend, 'getQuota').mockResolvedValue({
    connected: true,
    session: { used: 0, limit: 100, percentage: 0 },
  });
  vi.spyOn(backend, 'getCodexInfo').mockResolvedValue({ connected: true });
  vi.spyOn(backend, 'getCodexRateLimits').mockResolvedValue({
    connected: true,
    ordinaryUsageAllowed: true,
    primary: { usedPercent: 0, windowMinutes: 300, resetsAt },
  });
  vi.spyOn(backend, 'getCodexResetCredits').mockResolvedValue({ connected: true, availableCount: 0, credits: [] });
  vi.spyOn(backend, 'getCodexWeeklyQuota').mockResolvedValue({});
  vi.spyOn(backend, 'getCodexProfiles').mockResolvedValue({
    profiles: [], registryError: null, registryProvenance: 'none',
  } satisfies CodexProfilesResponse);
  vi.spyOn(backend, 'getCursorInfo').mockResolvedValue({ connected: false });
  vi.spyOn(backend, 'getGrokInfo').mockResolvedValue({ connected: false, products: [] });
  vi.spyOn(backend, 'getAntigravityInfo').mockResolvedValue({ connected: false, status: 'pending' });
  vi.spyOn(backend, 'pingClaudeWindow').mockResolvedValue({ kind: 'cliFailed', code: 'spawnFailed' });
  vi.spyOn(backend, 'pingCodexWindow').mockResolvedValue({ kind: 'cliFailed', code: 'spawnFailed' });
  vi.spyOn(backend, 'setDockVisibility').mockResolvedValue(undefined);
  vi.spyOn(backend, 'updateTrayIcon').mockResolvedValue(undefined);
}

beforeAll(() => {
  (globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;
});

beforeEach(() => {
  vi.useFakeTimers();
  (globalThis as Record<string, unknown>).localStorage = memoryStorage({
    'claude-quota-tab': 'claude',
    'claude-tray-enabled': 'false',
    'codex-tray-enabled': 'false',
    'cursor-tray-enabled': 'false',
    'grok-tray-enabled': 'false',
    'antigravity-tray-enabled': 'false',
  });
  installBackend();
});

afterEach(() => {
  vi.clearAllTimers();
  vi.useRealTimers();
  vi.restoreAllMocks();
  delete (globalThis as Record<string, unknown>).localStorage;
});

afterAll(() => {
  delete (globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT;
});

describe('App ping result and provider refresh isolation', () => {
  it('keeps a Claude result on its initiating tab and expires it after a tab round trip', async () => {
    const outcome = deferred<PingOutcome>();
    vi.mocked(backend.pingClaudeWindow).mockReturnValue(outcome.promise);
    const renderer = await renderApp();
    vi.mocked(backend.getQuota).mockClear();
    vi.mocked(backend.getCodexInfo).mockClear();

    await act(async () => {
      pingButton(renderer).props.onClick();
      await flush();
    });
    await clickProvider(renderer, 'codex');
    await act(async () => {
      outcome.resolve({ kind: 'opened', resetsAt: 1_800_000_000, tokens: null, confirmedAfterSecs: 0 });
      await flush();
    });
    expect(text(renderer)).not.toContain('Window started');
    expect(backend.getQuota).toHaveBeenCalledTimes(1);
    expect(backend.getCodexInfo).not.toHaveBeenCalled();

    await clickProvider(renderer, 'claude');
    expect(text(renderer)).toContain('Window started');
    await act(async () => {
      vi.advanceTimersByTime(8_000);
      await flush();
    });
    await clickProvider(renderer, 'codex');
    await clickProvider(renderer, 'claude');
    expect(text(renderer)).not.toContain('Window started');
    await act(async () => renderer.unmount());
  });

  it('refreshes Codex once after a Codex ping without refreshing Claude', async () => {
    vi.mocked(backend.pingCodexWindow).mockResolvedValue({
      kind: 'opened', resetsAt: 1_800_000_000, tokens: 3, confirmedAfterSecs: 0,
    });
    const renderer = await renderApp();
    await clickProvider(renderer, 'codex');
    vi.mocked(backend.getQuota).mockClear();
    vi.mocked(backend.getCodexInfo).mockClear();
    vi.mocked(backend.getCodexRateLimits).mockClear();
    vi.mocked(backend.getCodexProfiles).mockClear();

    await act(async () => {
      pingButton(renderer).props.onClick();
      await flush();
    });
    expect(backend.pingCodexWindow).toHaveBeenCalledWith('default', false);
    expect(backend.getCodexInfo).toHaveBeenCalledTimes(1);
    expect(backend.getCodexRateLimits).toHaveBeenCalledTimes(1);
    expect(backend.getCodexProfiles).toHaveBeenCalledTimes(1);
    expect(backend.getQuota).not.toHaveBeenCalled();
    await act(async () => renderer.unmount());
  });

  it('isolates custom-profile results and permits two target pings concurrently', async () => {
    const resetsAt = Math.floor(Date.now() / 1000) + 5 * 60 * 60;
    vi.mocked(backend.getCodexProfiles).mockResolvedValue({
      profiles: [{
        alias: 'work', status: 'connected', primary: { usedPercent: 0, windowMinutes: 300, resetsAt },
        availableResetCredits: 0, ordinaryUsageAllowed: true,
      }], registryError: null, registryProvenance: 'primary',
    });
    const defaultOutcome = deferred<PingOutcome>();
    const workOutcome = deferred<PingOutcome>();
    vi.mocked(backend.pingCodexWindow).mockImplementation((alias) => (
      alias === 'default' ? defaultOutcome.promise : workOutcome.promise
    ));
    const renderer = await renderApp();
    await clickProvider(renderer, 'codex');
    await act(async () => {
      pingButton(renderer).props.onClick();
      await flush();
    });
    await act(async () => {
      accountTab(renderer, 'work').props.onClick();
      await flush();
    });
    expect(pingButton(renderer).props.disabled).toBe(false);
    await act(async () => {
      pingButton(renderer).props.onClick();
      await flush();
    });
    expect(backend.pingCodexWindow).toHaveBeenCalledWith('default', false);
    expect(backend.pingCodexWindow).toHaveBeenCalledWith('work', false);
    await act(async () => {
      defaultOutcome.resolve({ kind: 'cliFailed', code: 'spawnFailed' });
      await flush();
    });
    expect(text(renderer)).not.toContain('Ping failed · spawnFailed');
    await act(async () => {
      workOutcome.resolve({ kind: 'opened', resetsAt: 1_800_000_000, tokens: null, confirmedAfterSecs: 0 });
      await flush();
    });
    expect(text(renderer)).toContain('Window started');
    await act(async () => {
      accountTab(renderer, 'Default').props.onClick();
      await flush();
    });
    expect(text(renderer)).toContain('Ping failed · spawnFailed');
    expect(text(renderer)).not.toContain('Window started');
    await act(async () => renderer.unmount());
  });

  it('clears a frozen confirmation when the Codex account tab changes', async () => {
    const resetsAt = Math.floor(Date.now() / 1000) + 5 * 60 * 60;
    vi.mocked(backend.getCodexRateLimits).mockResolvedValue({
      connected: true, ordinaryUsageAllowed: true,
      primary: { usedPercent: 1, windowMinutes: 300, resetsAt },
    });
    vi.mocked(backend.getCodexProfiles).mockResolvedValue({
      profiles: [{
        alias: 'work', status: 'connected', primary: { usedPercent: 0, windowMinutes: 300, resetsAt },
        availableResetCredits: 0, ordinaryUsageAllowed: true,
      }], registryError: null, registryProvenance: 'primary',
    });
    const renderer = await renderApp();
    await clickProvider(renderer, 'codex');
    await act(async () => {
      pingButton(renderer).props.onClick();
      await flush();
    });
    expect(text(renderer)).toContain('Send a ping anyway?');
    await act(async () => {
      accountTab(renderer, 'work').props.onClick();
      await flush();
    });
    expect(text(renderer)).not.toContain('Send a ping anyway?');
    expect(backend.pingCodexWindow).not.toHaveBeenCalled();
    await act(async () => renderer.unmount());
  });

  it('keeps a captured confirmation Send bound to its original Codex target after a tab switch', async () => {
    const resetsAt = Math.floor(Date.now() / 1000) + 5 * 60 * 60;
    vi.mocked(backend.getCodexRateLimits).mockResolvedValue({
      connected: true, ordinaryUsageAllowed: true,
      primary: { usedPercent: 1, windowMinutes: 300, resetsAt },
    });
    const renderer = await renderApp();
    await clickProvider(renderer, 'codex');
    await act(async () => {
      pingButton(renderer).props.onClick();
      await flush();
    });
    expect(text(renderer)).toContain('Send a ping anyway?');
    const capturedSend = renderer.root.find(
      (node) => node.type === 'button' && node.children.join('') === 'Send',
    ).props.onClick;

    await clickProvider(renderer, 'claude');
    expect(text(renderer)).not.toContain('Send a ping anyway?');
    expect(backend.pingClaudeWindow).not.toHaveBeenCalled();

    await act(async () => {
      capturedSend();
      await flush();
    });
    expect(backend.pingCodexWindow).toHaveBeenCalledWith('default', true);
    expect(backend.pingClaudeWindow).not.toHaveBeenCalled();
    await act(async () => renderer.unmount());
  });

  it('drops a delayed confirmation-required outcome after its target tab changes', async () => {
    const outcome = deferred<PingOutcome>();
    vi.mocked(backend.pingCodexWindow)
      .mockReturnValueOnce(outcome.promise)
      .mockResolvedValueOnce({ kind: 'cliFailed', code: 'spawnFailed' });
    const renderer = await renderApp();
    await clickProvider(renderer, 'codex');

    await act(async () => {
      pingButton(renderer).props.onClick();
      await flush();
    });
    expect(backend.pingCodexWindow).toHaveBeenCalledWith('default', false);
    // Complete the tab round trip before the outcome lands: the clearing effect
    // has already run, so only the generation check can drop the late outcome.
    await clickProvider(renderer, 'claude');
    await clickProvider(renderer, 'codex');
    await act(async () => {
      outcome.resolve({ kind: 'confirmationRequired' });
      await flush();
    });

    expect(text(renderer)).not.toContain("Couldn't confirm window state. Send a ping anyway?");
    await act(async () => {
      pingButton(renderer).props.onClick();
      await flush();
    });
    expect(backend.pingCodexWindow).toHaveBeenCalledTimes(2);
    await act(async () => renderer.unmount());
  });

  it('keeps a confirmation-required outcome on its unchanged target tab', async () => {
    const outcome = deferred<PingOutcome>();
    vi.mocked(backend.pingCodexWindow).mockReturnValue(outcome.promise);
    const renderer = await renderApp();
    await clickProvider(renderer, 'codex');

    await act(async () => {
      pingButton(renderer).props.onClick();
      await flush();
      outcome.resolve({ kind: 'confirmationRequired' });
      await flush();
    });

    expect(text(renderer)).toContain("Couldn't confirm window state. Send a ping anyway?");
    await act(async () => renderer.unmount());
  });

  it('does not let an old result timer clear a newer result for the same target', async () => {
    const renderer = await renderApp();
    await act(async () => {
      pingButton(renderer).props.onClick();
      await flush();
    });
    expect(text(renderer)).toContain('Ping failed · spawnFailed');
    await act(async () => {
      vi.advanceTimersByTime(4_000);
      pingButton(renderer).props.onClick();
      await flush();
    });
    await act(async () => {
      vi.advanceTimersByTime(4_001);
      await flush();
    });
    expect(text(renderer)).toContain('Ping failed · spawnFailed');
    await act(async () => {
      vi.advanceTimersByTime(3_999);
      await flush();
    });
    expect(text(renderer)).not.toContain('Ping failed · spawnFailed');
    await act(async () => renderer.unmount());
  });
});
