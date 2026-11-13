import { createElement } from 'react';
import { act, create, type ReactTestRenderer } from 'react-test-renderer';
import { afterAll, beforeAll, describe, expect, it, vi } from 'vitest';
import ClaudePanel from '../src/components/ClaudePanel';
import App from '../src/App';
import { backend } from '../src/services/backend';
import type { ClaudeProfile, ClaudeProfileStatus, QuotaData } from '../src/types/models';

vi.mock('../src/hooks/use_popover_window', () => ({ usePopoverWindow: () => false }));

const quota = {
  connected: true, session: { used: 37, limit: 100, percentage: 37, resetTime: null },
  weeklyTotal: null, weeklyOpus: null, weeklySonnet: null, weeklyDesign: null,
  weeklyFable5: null, error: null,
  account: { tail: 'd5fb0b', email: 'test@example.test' },
  accountLabel: 'test@example.test',
  appAccount: { state: 'same' },
} as unknown as QuotaData;

const profile = (status: ClaudeProfileStatus): ClaudeProfile => ({
  alias: 'lucky', label: 'Lucky', status,
  account: status === 'ok' ? { tail: '8c6913', email: 'lucky@example.test' } : null,
  quota: status === 'ok' ? { ...quota, account: { tail: '8c6913', email: 'lucky@example.test' }, accountLabel: undefined, appAccount: undefined }
    : status === 'unidentified' ? { ...quota, account: undefined, accountLabel: undefined, appAccount: undefined } : null,
  ...(status === 'signedOut' ? { expectedServiceName: 'Claude Code-credentials-2becd79b' } : {}),
});

async function render(status?: ClaudeProfileStatus, appAccount = quota.appAccount): Promise<ReactTestRenderer> {
  let result!: ReactTestRenderer;
  await act(async () => {
    result = create(createElement(ClaudePanel, {
      quota: { ...quota, appAccount }, loading: false, error: null, windowVisible: false,
      costRefreshKey: 0, onRetry: () => {}, selectedAlias: status ? 'lucky' : 'default',
      profiles: { profiles: status ? [profile(status)] : [] },
      sections: { timeline: false, cost: false, trend: false, tips: false },
    }));
  });
  return result;
}

beforeAll(() => { (globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true; });
afterAll(() => { delete (globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT; });

describe('Claude account tabs', () => {
  it('does not show tabs without custom profiles', async () => {
    const view = await render();
    expect(view.root.findAllByProps({ role: 'tab' })).toHaveLength(0);
    await act(async () => view.unmount());
  });
  it.each([
    ['disabled', 'Not enabled (directory missing)'], ['signedOut', 'Signed out'],
    ['expired', 'Login expired'], ['mismatch', 'Account mismatch'],
    ['unidentified', 'Account unidentified'], ['ok', 'Connected'],
  ] as const)('shows the %s status', async (status, label) => {
    const view = await render(status);
    expect(JSON.stringify(view.toJSON())).toContain(label);
    expect(view.root.findAllByProps({ role: 'tab' })).toHaveLength(2);
    if (status === 'mismatch') expect(JSON.stringify(view.toJSON())).not.toContain('37%');
    if (status === 'disabled') expect(view.root.findAllByProps({ className: 'retry-btn' })).toHaveLength(0);
    await act(async () => view.unmount());
  });
  it.each(['same', 'unknown'] as const)('does not show app drift when %s', async (state) => {
    const view = await render(undefined, { state });
    expect(JSON.stringify(view.toJSON())).not.toContain('Desktop app is signed in');
    await act(async () => view.unmount());
  });
  it('shows the app drift only for different accounts', async () => {
    const view = await render(undefined, { state: 'different', appLabel: 'Lucky' });
    expect(JSON.stringify(view.toJSON())).toContain('Desktop app is signed in as ');
    expect(JSON.stringify(view.toJSON())).toContain('Lucky');
    await act(async () => view.unmount());
  });
  it('shows an unavailable refresh result on the selected profile', async () => {
    let view!: ReactTestRenderer;
    await act(async () => {
      view = create(createElement(ClaudePanel, {
        quota, loading: false, error: null, profileRefreshError: 'Claude profile unavailable.',
        windowVisible: false, costRefreshKey: 0, onRetry: () => {}, selectedAlias: 'lucky',
        profiles: { profiles: [profile('ok')] },
        sections: { timeline: false, cost: false, trend: false, tips: false },
      }));
    });
    expect(JSON.stringify(view.toJSON())).toContain('Claude profile unavailable.');
    await act(async () => view.unmount());
  });
  it('sends Ping to the selected profile alias', async () => {
    const values = new Map([['claude-quota-tab', 'claude']]);
    (globalThis as Record<string, unknown>).localStorage = {
      getItem: (key: string) => values.get(key) ?? null,
      setItem: (key: string, value: string) => values.set(key, value),
      removeItem: (key: string) => values.delete(key),
      clear: () => values.clear(), key: (index: number) => [...values.keys()][index] ?? null,
      get length() { return values.size; },
    };
    vi.spyOn(backend, 'setClaudeAutoRenew').mockResolvedValue(undefined);
    vi.spyOn(backend, 'getQuota').mockResolvedValue(quota);
    vi.spyOn(backend, 'getClaudeProfiles').mockResolvedValue({ profiles: [{
      alias: 'lucky', label: 'Lucky', status: 'ok', account: { tail: '8c6913', email: 'lucky@example.test' },
      quota: { ...quota, session: { used: 0, limit: 100, percentage: 0, resetTime: null } as unknown as QuotaData['session'], account: { tail: '8c6913', email: 'lucky@example.test' }, accountLabel: undefined, appAccount: undefined },
    }] });
    const ping = vi.spyOn(backend, 'pingClaudeWindow').mockResolvedValue({ kind: 'busy' });
    let view!: ReactTestRenderer;
    await act(async () => { view = create(createElement(App)); await Promise.resolve(); await Promise.resolve(); await Promise.resolve(); });
    await act(async () => { view.root.find((node) => node.props.role === 'tab' && node.children.join('') === 'Lucky').props.onClick(); });
    await act(async () => { view.root.find((node) => node.type === 'button' && node.props.className === 'action-btn ping-btn').props.onClick(); await Promise.resolve(); });
    expect(ping).toHaveBeenCalledWith('lucky', false);
    await act(async () => view.unmount());
    vi.restoreAllMocks();
    delete (globalThis as Record<string, unknown>).localStorage;
  });
  it('keeps the latest Claude profile response when requests finish out of order', async () => {
    const values = new Map([['claude-quota-tab', 'claude']]);
    (globalThis as Record<string, unknown>).localStorage = {
      getItem: (key: string) => values.get(key) ?? null,
      setItem: (key: string, value: string) => values.set(key, value),
      removeItem: (key: string) => values.delete(key),
      clear: () => values.clear(), key: (index: number) => [...values.keys()][index] ?? null,
      get length() { return values.size; },
    };
    vi.spyOn(backend, 'setClaudeAutoRenew').mockResolvedValue(undefined);
    vi.spyOn(backend, 'getQuota').mockResolvedValue(quota);
    let first!: (value: { profiles: ClaudeProfile[] }) => void;
    let second!: (value: { profiles: ClaudeProfile[] }) => void;
    const oldRequest = new Promise<{ profiles: ClaudeProfile[] }>((resolve) => { first = resolve; });
    const newRequest = new Promise<{ profiles: ClaudeProfile[] }>((resolve) => { second = resolve; });
    vi.spyOn(backend, 'getClaudeProfiles').mockReturnValueOnce(oldRequest).mockReturnValueOnce(newRequest);
    let view!: ReactTestRenderer;
    await act(async () => { view = create(createElement(App)); await Promise.resolve(); await Promise.resolve(); });
    await act(async () => {
      view.root.findByProps({ 'aria-label': 'Refresh current provider' }).props.onClick();
      await Promise.resolve(); await Promise.resolve(); await Promise.resolve();
    });
    await act(async () => { second({ profiles: [{ ...profile('ok'), alias: 'new', label: 'Newest' }] }); await Promise.resolve(); });
    await act(async () => { first({ profiles: [{ ...profile('ok'), alias: 'old', label: 'Obsolete' }] }); await Promise.resolve(); });
    const rendered = JSON.stringify(view.toJSON());
    expect(rendered).toContain('Newest');
    expect(rendered).not.toContain('Obsolete');
    await act(async () => view.unmount());
    vi.restoreAllMocks();
    delete (globalThis as Record<string, unknown>).localStorage;
  });
});
