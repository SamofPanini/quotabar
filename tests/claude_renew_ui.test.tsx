import { createElement } from 'react';
import { act, create, type ReactTestRenderer } from 'react-test-renderer';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import ClaudePanel from '../src/components/ClaudePanel';
import OverviewPanel from '../src/components/OverviewPanel';
import SettingsView from '../src/components/SettingsView';
import { formatServiceStatusDate } from '../src/services/service_status';
import type { QuotaData } from '../src/types/models';

function memoryStorage() {
  const values = new Map<string, string>();
  return {
    getItem: (key: string) => values.get(key) ?? null,
    setItem: (key: string, value: string) => values.set(key, value),
    removeItem: (key: string) => values.delete(key),
    clear: () => values.clear(),
    key: (index: number) => Array.from(values.keys())[index] ?? null,
    get length() { return values.size; },
  };
}

function staleQuota(
  reason: 'expired' | 'signed_out',
  resetTime = '2026-10-11T14:00:00Z',
  autoRenew?: 'pending' | 'sent' | 'skipped',
): QuotaData {
  return {
    connected: true,
    staleSince: '2026-10-11T09:18:00Z',
    staleReason: reason,
    autoRenew,
    error: reason === 'expired' ? 'Claude Code login expired. Press Ping to renew it.' : 'Claude Code is signed out. Run claude auth login in Terminal.',
    session: { used: 14, limit: 100, percentage: 14, resetTime },
    weeklyTotal: { used: 42, limit: 100, percentage: 42, resetTime: '2026-10-15T09:18:00Z' },
    weeklyOpus: { used: 9, limit: 100, percentage: 9, resetTime: '2026-10-15T09:18:00Z' },
  };
}

beforeEach(() => {
  (globalThis as Record<string, unknown>).localStorage = memoryStorage();
});

afterEach(() => {
  vi.restoreAllMocks();
  delete (globalThis as Record<string, unknown>).localStorage;
});

describe('Claude renew UI', () => {
  it('renders all stale notices, gray cards, and every retained quota card', async () => {
    const current = Date.now;
    Date.now = () => Date.parse('2026-10-11T10:00:00Z');
    try {
      let automatic!: ReactTestRenderer;
      let manual!: ReactTestRenderer;
      let signedOut!: ReactTestRenderer;
      await act(async () => {
        automatic = create(createElement(ClaudePanel, {
          quota: staleQuota('expired', undefined, 'pending'), loading: false, error: staleQuota('expired').error!,
          windowVisible: false, costRefreshKey: 0, onRetry: () => {},
        }));
        manual = create(createElement(ClaudePanel, {
          quota: staleQuota('expired', '2026-10-11T09:00:00Z'), loading: false, error: staleQuota('expired').error!,
          windowVisible: false, costRefreshKey: 0, onRetry: () => {},
        }));
        signedOut = create(createElement(ClaudePanel, {
          quota: staleQuota('signed_out'), loading: false, error: staleQuota('signed_out').error!,
          windowVisible: false, costRefreshKey: 0, onRetry: () => {},
        }));
      });
      expect(JSON.stringify(automatic.toJSON())).toContain('renewing automatically');
      expect(JSON.stringify(manual.toJSON())).toContain('open Claude Code or press Ping to renew');
      expect(JSON.stringify(signedOut.toJSON())).toContain('sign in with claude in Terminal');
      const rendered = JSON.stringify(automatic.toJSON());
      for (const value of ['5-hour window', 'All models', 'Opus', '14%', '42%', '9%', 'Resets in']) {
        expect(rendered).toContain(value);
      }
      expect(automatic.root.findAll((node) => String(node.props.className).includes('quota-card-stale'))).not.toHaveLength(0);
    } finally {
      Date.now = current;
    }
  });

  it('shows automatic renewal wording only for backend pending or sent states', async () => {
    let pending!: ReactTestRenderer;
    let sent!: ReactTestRenderer;
    let skipped!: ReactTestRenderer;
    await act(async () => {
      pending = create(createElement(ClaudePanel, { quota: staleQuota('expired', undefined, 'pending'), loading: false, error: null, windowVisible: false, costRefreshKey: 0, onRetry: () => {} }));
      sent = create(createElement(ClaudePanel, { quota: staleQuota('expired', undefined, 'sent'), loading: false, error: null, windowVisible: false, costRefreshKey: 0, onRetry: () => {} }));
      skipped = create(createElement(ClaudePanel, { quota: staleQuota('expired', undefined, 'skipped'), loading: false, error: null, windowVisible: false, costRefreshKey: 0, onRetry: () => {} }));
    });
    expect(JSON.stringify(pending.toJSON())).toContain('renewing automatically');
    expect(JSON.stringify(sent.toJSON())).toContain('renewing automatically');
    expect(JSON.stringify(skipped.toJSON())).toContain('open Claude Code or press Ping to renew');
  });

  it('grays the Claude row and shows its stale reason on the All page', async () => {
    let renderer!: ReactTestRenderer;
    await act(async () => {
      renderer = create(createElement(OverviewPanel, {
        summaries: [],
        mostConstrained: [{ provider: 'claude', providerLabel: 'Claude Code', label: '5-hour usage', usedPercent: 14, resetLabel: '4h', resetAtMs: Date.now() + 1 }],
        upcomingResets: [], costRefreshKey: 0, showCostSummary: false, onProviderSelect: () => {},
        claudeStaleReason: 'expired',
      }));
    });
    expect(JSON.stringify(renderer.toJSON())).toContain('expired');
    expect(renderer.root.findAll((node) => String(node.props.className).includes('quota-card-stale'))).toHaveLength(1);
  });

  it('defaults the Auto-renew Claude login setting to on and persists a toggle', async () => {
    let renderer!: ReactTestRenderer;
    await act(async () => {
      renderer = create(createElement(SettingsView, {
        isMacOS: false, theme: 'light', dockHidden: true, trayEntries: [],
        panelSections: { tips: true, timeline: true, cost: true, trend: true },
        trayStyle: 'percent', trayCycle: false, menuBarQuotaWindow: 'weekly', claudeMenuBarQuotaWindow: 'weekly',
        events: [], notificationSettings: {
          q80: false, q95: false, q100: false, bonusReady: false, bonus: false, serviceStatus: false,
        },
        serviceStatusEnabled: false,
        switcherVisibility: { claude: true, codex: true, cursor: true, grok: true, antigravity: true },
        onClose: () => {}, onThemeChange: () => {}, onDockToggle: () => {}, onTrayToggle: () => {},
        onPanelSectionToggle: () => {}, onTrayStyleChange: () => {}, onTrayCycleToggle: () => {},
        onMenuBarQuotaWindowChange: () => {}, onClaudeMenuBarQuotaWindowChange: () => {},
        onNotificationToggle: () => {}, onServiceStatusToggle: () => {}, onSwitcherToggle: () => {},
        onApplyPreset: () => {}, onSelectEventProvider: () => {},
      }));
      await Promise.resolve();
    });
    const control = renderer.root.findByProps({ 'aria-label': 'Auto-renew Claude login' });
    expect(control.props['aria-checked']).toBe(true);
    await act(async () => { control.props.onClick(); });
    expect(localStorage.getItem('quotabar-claude-auto-renew')).toBe('false');
    expect(renderer.root.findByProps({ 'aria-label': 'Auto-renew Claude login' }).props['aria-checked']).toBe(false);
  });

  it('formats the retained-reading date in a fixed timezone', () => {
    expect(formatServiceStatusDate(
      '2026-10-11T09:18:00Z',
      new Date('2026-10-11T12:00:00Z'),
      'UTC',
    )).toBe('Oct 11, 09:18');
  });
});
