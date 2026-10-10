import { createElement } from 'react';
import { act, create, type ReactTestRenderer } from 'react-test-renderer';
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';
import App from '../src/App';
import { backend } from '../src/services/backend';
import type { ServiceStatusSnapshot } from '../src/services/service_status';
import type { CodexProfilesResponse, PingConfirmationEvent, PingOutcome } from '../src/types/models';

let pingConfirmationHandler: ((event: PingConfirmationEvent) => void) | undefined;

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
  vi.spyOn(backend, 'getServiceStatus').mockResolvedValue({
    claude: { provider: 'claude', level: 'operational', components: [], incidents: [], maintenances: [] },
    codex: { provider: 'codex', level: 'operational', components: [], incidents: [], maintenances: [] },
  });
  vi.spyOn(backend, 'setServiceStatusPrefs').mockResolvedValue(undefined);
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
  vi.spyOn(backend, 'onPingConfirmation').mockImplementation(async (handler) => {
    pingConfirmationHandler = handler;
    return () => { pingConfirmationHandler = undefined; };
  });
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
  pingConfirmationHandler = undefined;
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
  it('syncs service-status defaults and both settings toggles to the backend', async () => {
    const renderer = await renderApp();
    expect(backend.setServiceStatusPrefs).toHaveBeenCalledWith(true, true);

    await act(async () => {
      renderer.root.findByProps({ 'aria-label': 'Open settings' }).props.onClick();
      await flush();
    });
    await act(async () => {
      renderer.root.findByProps({ 'aria-label': 'Service status' }).props.onClick();
      await flush();
    });
    expect(backend.setServiceStatusPrefs).toHaveBeenLastCalledWith(false, true);

    await act(async () => {
      renderer.root.findByProps({ 'aria-label': 'Service status changes' }).props.onClick();
      await flush();
    });
    expect(backend.setServiceStatusPrefs).toHaveBeenLastCalledWith(false, false);
    await act(async () => renderer.unmount());
  });

  it('cleans a late service-status listener without applying its event', async () => {
    let resolveListen!: (stop: () => void) => void;
    let lateHandler!: (snapshot: ServiceStatusSnapshot) => void;
    const stop = vi.fn();
    const lateListen = new Promise<() => void>((resolve) => { resolveListen = resolve; });
    const serviceStatus = await import('../src/services/service_status');
    vi.spyOn(serviceStatus, 'onServiceStatusChanged').mockImplementation((handler) => {
      lateHandler = handler as typeof lateHandler;
      return lateListen;
    });
    const renderer = await renderApp();
    await act(async () => {
      renderer.root.findByProps({ 'aria-label': 'Open settings' }).props.onClick();
      await flush();
    });
    await act(async () => {
      renderer.root.findByProps({ 'aria-label': 'Service status' }).props.onClick();
      await flush();
    });
    await act(async () => { resolveListen(stop); await flush(); });
    await act(async () => {
      lateHandler({
        claude: { provider: 'claude', level: 'degraded', components: [], incidents: [], maintenances: [] },
        codex: { provider: 'codex', level: 'operational', components: [], incidents: [], maintenances: [] },
      });
      await flush();
    });
    await act(async () => {
      renderer.root.findByProps({ 'aria-label': 'Back to provider view' }).props.onClick();
      await flush();
    });
    expect(stop).toHaveBeenCalledOnce();
    expect(renderer.root.findAll((node) => node.props.className === 'service-status-notice degraded')).toHaveLength(0);
    await act(async () => renderer.unmount());
  });

  it('shows the Codex service notice below the header for a custom profile', async () => {
    const resetsAt = Math.floor(Date.now() / 1000) + 5 * 60 * 60;
    vi.mocked(backend.getServiceStatus).mockResolvedValue({
      claude: { provider: 'claude', level: 'operational', components: [], incidents: [], maintenances: [] },
      codex: { provider: 'codex', level: 'degraded', components: [], incidents: [{ id: 'i', name: 'Codex incident', status: 'monitoring' }], maintenances: [] },
    });
    vi.mocked(backend.getCodexProfiles).mockResolvedValue({
      profiles: [{ alias: 'work', status: 'connected', primary: { usedPercent: 1, windowMinutes: 300, resetsAt }, availableResetCredits: 0, ordinaryUsageAllowed: true }], registryError: null, registryProvenance: 'primary',
    });
    const renderer = await renderApp();
    await clickProvider(renderer, 'codex');
    await act(async () => { accountTab(renderer, 'work').props.onClick(); await flush(); });
    const nodes = renderer.root.findAll((node) => node.props.className === 'service-status-notice degraded');
    expect(nodes).toHaveLength(1);
    expect(text(renderer)).toContain('"OpenAI",": ","degraded"');
    const header = renderer.root.findAll((node) => node.props.className === 'provider-detail-header')[0];
    const allNodes = renderer.root.findAll(() => true);
    expect(allNodes.indexOf(header)).toBeLessThan(allNodes.indexOf(nodes[0]));
    await act(async () => renderer.unmount());
  });

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

  it('keeps confirming visible, then applies only the matching completion event', async () => {
    vi.mocked(backend.pingClaudeWindow).mockResolvedValue({
      kind: 'confirming', tokens: 3, expectedResetsAt: 1_800_018_000,
    });
    const renderer = await renderApp();
    vi.mocked(backend.getQuota).mockClear();

    await act(async () => {
      pingButton(renderer).props.onClick();
      await flush();
    });
    expect(text(renderer)).toContain('Ping sent · confirming…');
    expect(pingButton(renderer).props.disabled).toBe(true);
    expect(pingButton(renderer).props.title).toBe('Confirming window…');
    expect(pingButton(renderer).findByProps({ className: 'btn-text' }).children).toContain('Ping');
    await act(async () => {
      vi.advanceTimersByTime(8_000);
      await flush();
    });
    expect(text(renderer)).toContain('Ping sent · confirming…');
    expect(backend.getQuota).toHaveBeenCalledTimes(1);

    await act(async () => {
      pingConfirmationHandler?.({
        provider: 'codex', alias: 'default',
        outcome: { kind: 'opened', resetsAt: 1_800_000_000, tokens: null, confirmedAfterSecs: 30 },
      });
      await flush();
    });
    expect(text(renderer)).toContain('Ping sent · confirming…');
    expect(pingButton(renderer).props.disabled).toBe(true);

    vi.mocked(backend.getQuota).mockClear();
    await act(async () => {
      pingConfirmationHandler?.({
        provider: 'claude', alias: 'default',
        outcome: { kind: 'opened', resetsAt: 1_800_000_000, tokens: null, confirmedAfterSecs: 30 },
      });
      await flush();
    });
    expect(text(renderer)).toContain('Window started · resets');
    expect(pingButton(renderer).props.disabled).toBe(false);
    expect(backend.getQuota).toHaveBeenCalledTimes(1);
    await act(async () => renderer.unmount());
  });

  it('keeps the current Codex work target confirming when default completes', async () => {
    const resetsAt = Math.floor(Date.now() / 1000) + 5 * 60 * 60;
    vi.mocked(backend.getCodexProfiles).mockResolvedValue({
      profiles: [{
        alias: 'work', status: 'connected', primary: { usedPercent: 0, windowMinutes: 300, resetsAt },
        availableResetCredits: 0, ordinaryUsageAllowed: true,
      }], registryError: null, registryProvenance: 'primary',
    });
    vi.mocked(backend.pingCodexWindow).mockResolvedValue({
      kind: 'confirming', tokens: 3, expectedResetsAt: 1_800_018_000,
    });
    const renderer = await renderApp();
    await clickProvider(renderer, 'codex');
    await act(async () => {
      accountTab(renderer, 'work').props.onClick();
      await flush();
    });
    await act(async () => {
      pingButton(renderer).props.onClick();
      await flush();
    });
    expect(text(renderer)).toContain('Ping sent · confirming…');
    expect(pingButton(renderer).props.title).toBe('Confirming window…');

    await act(async () => {
      pingConfirmationHandler?.({
        provider: 'codex', alias: 'default',
        outcome: { kind: 'opened', resetsAt: 1_800_000_000, tokens: null, confirmedAfterSecs: 30 },
      });
      await flush();
    });
    expect(text(renderer)).toContain('Ping sent · confirming…');
    expect(pingButton(renderer).props.disabled).toBe(true);
    expect(pingButton(renderer).props.title).toBe('Confirming window…');
    await act(async () => renderer.unmount());
  });
});
