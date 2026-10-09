import { createElement } from 'react';
import { act, create, type ReactTestRenderer } from 'react-test-renderer';
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';
import CostSummarySection from '../src/components/CostSummarySection';
import SettingsView from '../src/components/SettingsView';
import { backend } from '../src/services/backend';
import { defaultNotificationSettings } from '../src/services/notifications';
import { defaultPanelSections } from '../src/services/panel_sections';
import { defaultSwitcherVisibility } from '../src/services/switcher_providers';
import { getCursorOnlineCostEnabled, setCursorOnlineCostEnabled } from '../src/services/storage';
import type { CostDailySeries, CostOverview } from '../src/types/models';

const tauri = vi.hoisted(() => ({ invoke: vi.fn(() => Promise.resolve()) }));

vi.mock('@tauri-apps/api/core', () => ({ invoke: tauri.invoke }));

const overview: CostOverview = {
  source: 'claude', displayName: 'Claude', currency: 'USD', generatedAt: '2026-10-09T00:00:00Z', cached: false, ranges: [],
};
const daily: CostDailySeries = {
  source: 'claude', currency: 'USD', generatedAt: '2026-10-09T00:00:00Z', cached: false, days: [],
};

function installStorage() {
  const values = new Map<string, string>();
  vi.stubGlobal('localStorage', {
    getItem: (key: string) => values.get(key) ?? null,
    setItem: (key: string, value: string) => values.set(key, value),
    removeItem: (key: string) => values.delete(key),
    clear: () => values.clear(),
  });
  return values;
}

beforeAll(() => {
  (globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;
});

beforeEach(() => {
  installStorage();
  tauri.invoke.mockClear();
});

afterEach(() => {
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

describe('Cursor online cost', () => {
  it('defaults off, persists the settings toggle, and fails closed on storage errors', async () => {
    expect(getCursorOnlineCostEnabled()).toBe(false);
    let renderer!: ReactTestRenderer;
    await act(async () => {
      renderer = create(createElement(SettingsView, {
        isMacOS: false, theme: 'light', dockHidden: false, trayEntries: [],
        panelSections: defaultPanelSections(), trayStyle: 'percent', trayCycle: false,
        menuBarQuotaWindow: 'weekly', claudeMenuBarQuotaWindow: 'weekly', events: [],
        notificationSettings: defaultNotificationSettings(), switcherVisibility: defaultSwitcherVisibility(),
        onClose: vi.fn(), onThemeChange: vi.fn(), onDockToggle: vi.fn(), onTrayToggle: vi.fn(),
        onPanelSectionToggle: vi.fn(), onTrayStyleChange: vi.fn(), onTrayCycleToggle: vi.fn(),
        onMenuBarQuotaWindowChange: vi.fn(), onClaudeMenuBarQuotaWindowChange: vi.fn(),
        onNotificationToggle: vi.fn(), onSwitcherToggle: vi.fn(), onApplyPreset: vi.fn(),
        onSelectEventProvider: vi.fn(),
      }));
    });
    const toggle = renderer.root.findByProps({ 'aria-label': 'Cursor online cost' });
    expect(toggle.props['aria-checked']).toBe(false);
    await act(async () => { toggle.props.onClick(); });
    expect(getCursorOnlineCostEnabled()).toBe(true);
    await act(async () => renderer.unmount());

    vi.stubGlobal('localStorage', { getItem: () => { throw new Error('unavailable'); } });
    vi.spyOn(console, 'error').mockImplementation(() => undefined);
    expect(getCursorOnlineCostEnabled()).toBe(false);
  });

  it('shows the exact off state and makes no Cursor cost requests', async () => {
    const overviewSpy = vi.spyOn(backend, 'getCostOverview').mockResolvedValue(overview);
    const dailySpy = vi.spyOn(backend, 'getCostDaily').mockResolvedValue(daily);
    let renderer!: ReactTestRenderer;
    await act(async () => {
      renderer = create(createElement(CostSummarySection, { source: 'cursor', autoRefreshIntervalMs: 0 }));
    });
    expect(JSON.stringify(renderer.toJSON())).toContain('Cursor online cost is off. Turn it on in Settings.');
    expect(overviewSpy).not.toHaveBeenCalled();
    expect(dailySpy).not.toHaveBeenCalled();
    await act(async () => renderer.unmount());
  });

  it('excludes disabled Cursor from overview totals and passes enabled state through IPC', async () => {
    const overviewSpy = vi.spyOn(backend, 'getCostOverview').mockResolvedValue(overview);
    const dailySpy = vi.spyOn(backend, 'getCostDaily').mockResolvedValue(daily);
    let renderer!: ReactTestRenderer;
    await act(async () => {
      renderer = create(createElement(CostSummarySection, {
        source: ['claude', 'cursor'], autoRefreshIntervalMs: 0,
      }));
    });
    expect(overviewSpy).toHaveBeenCalledExactlyOnceWith('claude', false, false);
    expect(dailySpy).toHaveBeenCalledExactlyOnceWith('claude', 30, false, false);
    expect(JSON.stringify(renderer.toJSON())).toContain('Cursor online cost is off. Turn it on in Settings.');
    await act(async () => renderer.unmount());

    vi.restoreAllMocks();
    vi.stubGlobal('window', { __TAURI_INTERNALS__: {} });
    await backend.getCostOverview('cursor', false, true);
    expect(tauri.invoke).toHaveBeenCalledWith('get_cost_overview', expect.objectContaining({
      source: 'cursor', allowCursorOnline: true,
    }));
  });
});
