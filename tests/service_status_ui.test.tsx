import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { act, create, type ReactTestRenderer } from 'react-test-renderer';
import { describe, expect, it, vi } from 'vitest';
import ClaudePanel from '../src/components/ClaudePanel';
import CodexPanel from '../src/components/CodexPanel';
import OverviewPanel from '../src/components/OverviewPanel';
import ServiceStatusNotice from '../src/components/ServiceStatusNotice';
import SettingsView from '../src/components/SettingsView';
import { backend } from '../src/services/backend';
import {
  claudePanelErrorWithServiceIncident,
  getServiceStatusEnabled,
  formatServiceStatusDate,
  orderedServiceEvents,
  serviceStatusForDisplay,
  type ProviderServiceStatus,
} from '../src/services/service_status';

const base = (level: ProviderServiceStatus['level']): ProviderServiceStatus => ({
  provider: 'claude', level, components: [], incidents: [], maintenances: [],
});
const event = { id: 'incident', name: 'Claude Console incident', status: 'monitoring', alert: 'warning' as const, url: 'https://status.example', latestUpdate: { id: 'update', status: 'monitoring', body: 'Working on it', updatedAt: '2026-10-10T12:34:00Z' } };
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
    const codex = { ...base('partial_outage'), provider: 'codex' as const, alert: 'warning' as const, incidents: [event] };
    const claudePanel = renderToStaticMarkup(createElement(ClaudePanel, {
      quota: null, loading: false, error: 'Quota request failed', windowVisible: true,
      costRefreshKey: 0, onRetry: vi.fn(), sections: hidden, serviceStatus: claude,
    }));
    const codexPanel = renderToStaticMarkup(createElement(CodexPanel, {
      sections: hidden, serviceStatus: codex,
    }));
    expect(claudePanel).toContain('[Monitoring]');
    expect(codexPanel).toContain('OpenAI');
    expect(codexPanel).toContain('[Monitoring]');
    expect(codexPanel).toContain('Claude Console incident');
    expect(codexPanel).toContain('service-status-notice warning');
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
    expect(rendered).toContain('Monitoring');
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
    expect(notice).toContain('[Monitoring]');
    expect(overview).toContain('Monitoring');
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

  it('formats service dates with a fixed timezone and year only when needed', () => {
    const now = new Date('2026-10-11T00:00:00Z');
    expect(formatServiceStatusDate('2026-10-07T13:25:04Z', now, 'Asia/Tokyo')).toBe('Oct 7, 22:25');
    expect(formatServiceStatusDate('2025-10-07T13:25:04Z', now, 'Asia/Tokyo')).toBe('Oct 7, 2025, 22:25');
  });

  it('renders a critical notice with a critical background and red stage pill', () => {
    const status = {
      ...base('degraded'), alert: 'critical' as const,
      incidents: [{ ...event, alert: 'critical' as const, status: 'identified' }],
    };
    const rendered = renderToStaticMarkup(createElement(ServiceStatusNotice, { provider: 'claude', status }));
    expect(rendered).toContain('service-status-notice critical');
    expect(rendered).toContain('service-status-stage critical');
  });

  it('orders a trailing critical event first and renders a more-status button', () => {
    const incidents = [
      { ...event, id: 'warning-one', name: 'Warning one', latestUpdate: { ...event.latestUpdate, updatedAt: '2026-10-10T12:00:00Z' } },
      { ...event, id: 'warning-two', name: 'Warning two', latestUpdate: { ...event.latestUpdate, updatedAt: '2026-10-10T13:00:00Z' } },
      { ...event, id: 'critical', name: 'Critical last', alert: 'critical' as const, status: 'identified', latestUpdate: { ...event.latestUpdate, updatedAt: '2026-10-10T11:00:00Z' } },
    ];
    const rendered = renderToStaticMarkup(createElement(ServiceStatusNotice, {
      provider: 'claude', status: { ...base('degraded'), alert: 'critical', incidents },
    }));
    expect(rendered).toContain('+1 more on the status page</button>');
    expect(rendered.indexOf('Critical last')).toBeLessThan(rendered.indexOf('Warning two'));
    expect(rendered).not.toContain('Warning one');
  });

  it('renders a component-only abnormal status without a time row', () => {
    const rendered = renderToStaticMarkup(createElement(ServiceStatusNotice, {
      provider: 'claude', status: {
        ...base('degraded'), alert: 'critical',
        components: [{ name: 'Claude Code', status: 'degraded_performance', tier: 'core' }],
      },
    }));
    expect(rendered).toContain('[Degraded performance]</span> Claude Code');
    expect(rendered).not.toContain('service-status-time');
  });

  it('includes event-affected operational components in the notice title', () => {
    const rendered = renderToStaticMarkup(createElement(ServiceStatusNotice, {
      provider: 'claude', status: {
        ...base('degraded'), alert: 'critical',
        components: [{ name: 'Claude Code (desktop)', status: 'operational', tier: 'core' }],
        incidents: [{ ...event, alert: 'critical', affectedComponents: ['Claude Code (desktop)'] }],
      },
    }));
    expect(rendered).toContain('Anthropic · Claude Code');
  });

  it('labels verifying maintenance as Maintenance', () => {
    const rendered = renderToStaticMarkup(createElement(ServiceStatusNotice, {
      provider: 'claude', status: {
        ...base('maintenance'), alert: 'none',
        maintenances: [{ ...event, alert: 'none', status: 'verifying' }],
      },
    }));
    expect(rendered).toContain('[Maintenance]');
    expect(rendered).not.toContain('[Verifying]');
  });

  it('uses the ordered event for the All-page critical pill and warning monitoring pill', () => {
    const criticalLast = {
      ...base('degraded'), alert: 'critical' as const,
      incidents: [event, { ...event, id: 'critical', alert: 'critical' as const, status: 'identified', name: 'Critical incident', affectedComponents: ['Claude Code'] }],
    };
    const warning = { ...base('degraded'), alert: 'warning' as const, incidents: [event] };
    const rendered = renderToStaticMarkup(createElement(OverviewPanel, {
      summaries: [], mostConstrained: [], upcomingResets: [], costRefreshKey: 0, onProviderSelect: vi.fn(), sections: hidden,
      serviceStatus: { claude: criticalLast, codex: { ...warning, provider: 'codex' as const } },
    }));
    expect(rendered).toContain('service-status-pill critical');
    expect(rendered).toContain('>Identified</span>');
    expect(rendered).toContain('service-status-pill warning');
    expect(rendered).toContain('>Monitoring</span>');
  });

  it('renders the 10-11 Claude timestamps in Asia/Tokyo', () => {
    const rendered = renderToStaticMarkup(createElement(ServiceStatusNotice, {
      provider: 'claude', timeZone: 'Asia/Tokyo', status: {
        ...base('degraded'), alert: 'warning',
        incidents: [{ ...event, startedAt: '2026-10-07T13:25:04Z', latestUpdate: { ...event.latestUpdate, updatedAt: '2026-10-07T21:44:41Z' } }],
      },
    }));
    expect(rendered).toContain('Started Oct 7, 22:25 · Updated Oct 8, 06:44');
  });

  it('renders the 10-11 Claude timestamps in UTC when requested', () => {
    const rendered = renderToStaticMarkup(createElement(ServiceStatusNotice, {
      provider: 'claude', timeZone: 'UTC', status: {
        ...base('degraded'), alert: 'warning',
        incidents: [{ ...event, startedAt: '2026-10-07T13:25:04Z', latestUpdate: { ...event.latestUpdate, updatedAt: '2026-10-07T21:44:41Z' } }],
      },
    }));
    expect(rendered).toContain('Started Oct 7, 13:25 · Updated Oct 7, 21:44');
  });

  it('orders same-alert events by update instant and puts invalid timestamps last', () => {
    const status = {
      ...base('degraded'),
      incidents: [
        { ...event, id: 'plus-ten', latestUpdate: { ...event.latestUpdate, updatedAt: '2026-10-07T10:00:00+10:00' } },
        { ...event, id: 'utc', latestUpdate: { ...event.latestUpdate, updatedAt: '2026-10-07T01:00:00Z' } },
        { ...event, id: 'invalid', latestUpdate: { ...event.latestUpdate, updatedAt: 'not-a-date' } },
      ],
    };
    expect(orderedServiceEvents(status).map((item) => item.id)).toEqual(['utc', 'plus-ten', 'invalid']);
  });

  it('shows only Started when the update is the same instant in another notation', () => {
    const rendered = renderToStaticMarkup(createElement(ServiceStatusNotice, {
      provider: 'claude', timeZone: 'UTC', status: {
        ...base('degraded'), alert: 'warning',
        incidents: [{ ...event, startedAt: '2026-10-07T13:25:04Z', latestUpdate: { ...event.latestUpdate, updatedAt: '2026-10-07T22:25:04+09:00' } }],
      },
    }));
    expect(rendered).toContain('Started Oct 7, 13:25');
    expect(rendered).not.toContain('Updated');
  });

  it('labels an incident with an empty phase as Incident', () => {
    const rendered = renderToStaticMarkup(createElement(ServiceStatusNotice, {
      provider: 'claude', status: {
        ...base('degraded'), alert: 'warning', incidents: [{ ...event, status: '' }],
      },
    }));
    expect(rendered).toContain('[Incident]');
    expect(rendered).not.toContain('[Maintenance]');
  });

  it('opens the first ordered event when the more-status button is pressed', async () => {
    const open = vi.spyOn(backend, 'openServiceStatusUrl').mockResolvedValue();
    let renderer!: ReactTestRenderer;
    await act(async () => { renderer = create(createElement(ServiceStatusNotice, {
      provider: 'claude', status: {
        ...base('degraded'), alert: 'critical',
        incidents: [
          { ...event, id: 'warning-one', url: 'https://status.example/warning-one' },
          { ...event, id: 'warning-two', url: 'https://status.example/warning-two' },
          { ...event, id: 'critical', alert: 'critical', url: 'https://status.example/critical' },
        ],
      },
    })); });
    const button = renderer.root.findAll((node) => node.props.className === 'service-status-more')[0];
    await act(async () => { button.props.onClick(); });
    expect(open).toHaveBeenCalledWith('https://status.example/critical');
    open.mockRestore();
  });
});
