import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { act, create, type ReactTestRenderer } from 'react-test-renderer';
import { describe, expect, it, vi } from 'vitest';
import ClaudePanel from '../src/components/ClaudePanel';
import CodexPanel from '../src/components/CodexPanel';
import OverviewPanel from '../src/components/OverviewPanel';
import ServiceStatusNotice from '../src/components/ServiceStatusNotice';
import SettingsView from '../src/components/SettingsView';
import {
  claudePanelErrorWithServiceIncident,
  getServiceStatusEnabled,
  serviceStatusForDisplay,
  type ProviderServiceStatus,
} from '../src/services/service_status';

const base = (level: ProviderServiceStatus['level']): ProviderServiceStatus => ({
  provider: 'claude', level, components: [], incidents: [], maintenances: [],
});
const event = { id: 'incident', name: 'Claude Console incident', status: 'monitoring', url: 'https://status.example', latestUpdate: { id: 'update', status: 'monitoring', body: 'Working on it', updatedAt: '2026-10-10T12:34:00Z' } };
const hidden = { timeline: false, cost: false, trend: false, tips: false };

function text(renderer: ReactTestRenderer) { return JSON.stringify(renderer.toJSON()); }

describe('service status UI', () => {
  it('renders all alert levels, hides operational, and marks Console renewal impact', () => {
    for (const level of ['degraded', 'partial_outage', 'major_outage', 'maintenance'] as const) {
      const status = { ...base(level), incidents: level === 'maintenance' ? [] : [event], maintenances: level === 'maintenance' ? [event] : [] };
      expect(renderToStaticMarkup(createElement(ServiceStatusNotice, { provider: 'claude', status }))).toContain('Anthropic');
    }
    expect(renderToStaticMarkup(createElement(ServiceStatusNotice, { provider: 'claude', status: base('operational') }))).toBe('');
    expect(renderToStaticMarkup(createElement(ServiceStatusNotice, { provider: 'claude', status: base('unknown') }))).toContain('Status unavailable');
    const consoleStatus = { ...base('degraded'), components: [{ name: 'Claude Console (platform.claude.com)', status: 'degraded_performance' }], incidents: [event] };
    expect(renderToStaticMarkup(createElement(ServiceStatusNotice, { provider: 'claude', status: consoleStatus }))).toContain('Affects login renewal');
  });

  it('shows provider notices even while Claude quota is absent and Codex is loading', () => {
    const claude = { ...base('degraded'), incidents: [event] };
    const codex = { ...base('partial_outage'), provider: 'codex' as const, incidents: [event] };
    const claudePanel = renderToStaticMarkup(createElement(ClaudePanel, {
      quota: null, loading: false, error: 'Quota request failed', windowVisible: true,
      costRefreshKey: 0, onRetry: vi.fn(), sections: hidden, serviceStatus: claude,
    }));
    const codexPanel = renderToStaticMarkup(createElement(CodexPanel, {
      sections: hidden, serviceStatus: codex,
    }));
    expect(claudePanel).toContain('Anthropic: degraded');
    expect(codexPanel).toContain('OpenAI: partial outage');
  });

  it('shows only Claude/Codex overview marks and appends the Claude incident hint', () => {
    const claude = { ...base('degraded'), incidents: [event] };
    const codex = { ...base('operational'), provider: 'codex' as const };
    const rendered = renderToStaticMarkup(createElement(OverviewPanel, {
      summaries: [], mostConstrained: [
        { provider: 'claude', providerLabel: 'Claude', label: '5-hour', usedPercent: 5 },
        { provider: 'codex', providerLabel: 'Codex', label: '5-hour', usedPercent: 6 },
        { provider: 'cursor', providerLabel: 'Cursor', label: 'Fast', usedPercent: 7 },
      ], upcomingResets: [], costRefreshKey: 0, onProviderSelect: vi.fn(), sections: hidden, serviceStatus: { claude, codex },
    }));
    expect(rendered).toContain('Degraded');
    expect(rendered).toContain('all watched services operational');
    expect(rendered).not.toContain('Cursor: all watched services operational');
    const panel = renderToStaticMarkup(createElement(ClaudePanel, { quota: { connected: true }, loading: false, error: 'Quota fetch failed. Anthropic reports an incident.', windowVisible: true, costRefreshKey: 0, onRetry: vi.fn(), sections: hidden, serviceStatus: claude }));
    expect(panel).toContain('Anthropic reports an incident.');
  });

  it('keeps a monitoring incident visible when its components are operational without an All-page green dot', () => {
    const claude = {
      ...base('degraded'),
      components: [{ name: 'Claude Console (platform.claude.com)', status: 'operational' }],
      incidents: [event],
    };
    const notice = renderToStaticMarkup(createElement(ServiceStatusNotice, { provider: 'claude', status: claude }));
    const overview = renderToStaticMarkup(createElement(OverviewPanel, {
      summaries: [], mostConstrained: [], upcomingResets: [], costRefreshKey: 0,
      onProviderSelect: vi.fn(), sections: hidden,
      serviceStatus: { claude, codex: { ...base('unknown'), provider: 'codex' as const } },
    }));
    expect(notice).toContain('Anthropic: degraded');
    expect(overview).toContain('Degraded');
    expect(overview).not.toContain('Anthropic: all watched services operational');
  });

  it('appends the incident hint through the App auth-error composition branch', () => {
    const claude = { ...base('degraded'), incidents: [event] };
    expect(claudePanelErrorWithServiceIncident(
      'Claude Code login expired.',
      true,
      'Claude Code login is still expired. Press Ping to renew it.',
      claude,
    )).toBe('Claude Code login is still expired. Press Ping to renew it. Anthropic reports an incident.');
  });

  it('clears the App display snapshot when service-status checking is disabled', () => {
    const snapshot = {
      claude: { ...base('degraded'), incidents: [event] },
      codex: { ...base('operational'), provider: 'codex' as const },
    };
    expect(serviceStatusForDisplay(false, snapshot)).toBeNull();
    expect(serviceStatusForDisplay(true, snapshot)).toBe(snapshot);
  });

  it('defaults the setting to enabled after a storage read failure and toggle is wired', async () => {
    const consoleError = vi.spyOn(console, 'error').mockImplementation(() => {});
    vi.stubGlobal('localStorage', {
      getItem: () => { throw new Error('storage unavailable'); },
      setItem: vi.fn(),
    });
    expect(getServiceStatusEnabled()).toBe(true);
    vi.unstubAllGlobals();
    consoleError.mockRestore();
    const toggle = vi.fn();
    let renderer!: ReactTestRenderer;
    await act(async () => { renderer = create(createElement(SettingsView, {
      isMacOS: true, theme: 'light', dockHidden: false, trayEntries: [], panelSections: hidden, trayStyle: 'percent', trayCycle: false, menuBarQuotaWindow: 'weekly', claudeMenuBarQuotaWindow: 'weekly', events: [],
      notificationSettings: { q80: true, q95: true, q100: true, bonusReady: true, bonus: true, serviceStatus: true }, serviceStatusEnabled: true,
      switcherVisibility: { claude: true, codex: true, cursor: true, grok: true, antigravity: true }, onClose: vi.fn(), onThemeChange: vi.fn(), onDockToggle: vi.fn(), onTrayToggle: vi.fn(), onPanelSectionToggle: vi.fn(), onTrayStyleChange: vi.fn(), onTrayCycleToggle: vi.fn(), onMenuBarQuotaWindowChange: vi.fn(), onClaudeMenuBarQuotaWindowChange: vi.fn(), onNotificationToggle: vi.fn(), onServiceStatusToggle: toggle, onSwitcherToggle: vi.fn(), onApplyPreset: vi.fn(), onSelectEventProvider: vi.fn(),
    })); });
    const button = renderer.root.findAll((node) => node.props['aria-label'] === 'Service status')[0];
    await act(async () => { button.props.onClick(); });
    expect(toggle).toHaveBeenCalledOnce();
  });
});
