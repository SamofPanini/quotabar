import { createElement } from 'react';
import { act, create, type ReactTestRenderer } from 'react-test-renderer';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import SettingsView from '../src/components/SettingsView';
import {
  AUTOSTART_STATUS_FAILURE_MESSAGE,
  AUTOSTART_UPDATE_FAILURE_MESSAGE,
} from '../src/services/autostart';
import type { TrayToggleEntry } from '../src/components/TrayToggles';

const autostart = vi.hoisted(() => ({
  readAutostartEnabled: vi.fn(),
  setAutostartEnabled: vi.fn(),
}));

vi.mock('../src/services/autostart', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../src/services/autostart')>();
  return {
    ...actual,
    readAutostartEnabled: autostart.readAutostartEnabled,
    setAutostartEnabled: autostart.setAutostartEnabled,
  };
});

const trayEntries: TrayToggleEntry[] = [
  { service: 'claude', label: 'Claude Tray', enabled: true, canDisable: true, connected: true, connectedHint: 'Ready', disconnectedHint: 'Sign in' },
];

function settingsProps(overrides: Partial<Parameters<typeof SettingsView>[0]> = {}) {
  return {
    isMacOS: true,
    theme: 'light' as const,
    dockHidden: false,
    trayEntries,
    panelSections: { timeline: true, cost: true, trend: true, tips: true },
    trayStyle: 'percent' as const,
    trayCycle: false,
    menuBarQuotaWindow: 'weekly' as const,
    claudeMenuBarQuotaWindow: 'weekly' as const,
    events: [],
    notificationSettings: { q80: true, q95: true, q100: true, bonusReady: true, bonus: false },
    switcherVisibility: { claude: true, codex: true, cursor: true, grok: true, antigravity: true },
    onClose: () => {},
    onThemeChange: () => {},
    onDockToggle: () => {},
    onTrayToggle: () => {},
    onPanelSectionToggle: () => {},
    onTrayStyleChange: () => {},
    onTrayCycleToggle: () => {},
    onMenuBarQuotaWindowChange: () => {},
    onClaudeMenuBarQuotaWindowChange: () => {},
    onNotificationToggle: () => {},
    onSwitcherToggle: () => {},
    onApplyPreset: () => {},
    onSelectEventProvider: () => {},
    ...overrides,
  };
}

function launchSwitch(renderer: ReactTestRenderer) {
  return renderer.root.findByProps({ 'aria-label': 'Launch at Login' });
}

function checkAgainButton(renderer: ReactTestRenderer) {
  return renderer.root.findByProps({ 'aria-label': 'Check Launch at Login status' });
}

beforeEach(() => {
  (globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;
  autostart.readAutostartEnabled.mockReset().mockResolvedValue({ status: 'ok', enabled: false });
  autostart.setAutostartEnabled.mockReset().mockResolvedValue({ status: 'ok', enabled: true });
});

afterEach(() => {
  vi.restoreAllMocks();
});

describe('Launch at Login settings row', () => {
  it('renders independent Codex and Claude window controls', async () => {
    const onCodexChange = vi.fn();
    const onClaudeChange = vi.fn();
    let renderer!: ReactTestRenderer;
    await act(async () => {
      renderer = create(createElement(SettingsView, settingsProps({
        menuBarQuotaWindow: 'five_hour',
        claudeMenuBarQuotaWindow: 'weekly',
        onMenuBarQuotaWindowChange: onCodexChange,
        onClaudeMenuBarQuotaWindowChange: onClaudeChange,
      })));
      await Promise.resolve();
    });

    const codexWindow = renderer.root.findByProps({ 'aria-label': 'Codex menu-bar icon window' });
    const claudeWindow = renderer.root.findByProps({ 'aria-label': 'Claude menu-bar icon window' });
    expect(codexWindow.findAllByType('button').map((button) => button.props['aria-pressed'])).toEqual([false, true]);
    expect(claudeWindow.findAllByType('button').map((button) => button.props['aria-pressed'])).toEqual([true, false]);

    await act(async () => claudeWindow.findAllByType('button')[1]?.props.onClick());
    expect(onClaudeChange).toHaveBeenCalledExactlyOnceWith('five_hour');
    expect(onCodexChange).not.toHaveBeenCalled();
    await act(async () => renderer.unmount());
  });

  it('reflects an existing OS login item after load', async () => {
    autostart.readAutostartEnabled.mockResolvedValue({ status: 'ok', enabled: true });
    let renderer!: ReactTestRenderer;
    await act(async () => {
      renderer = create(createElement(SettingsView, settingsProps()));
      await Promise.resolve();
    });

    expect(launchSwitch(renderer).props['aria-checked']).toBe(true);
    await act(async () => renderer.unmount());
  });

  it('shows Check again and status copy when the status cannot be read', async () => {
    autostart.readAutostartEnabled.mockResolvedValue({
      status: 'failure',
      message: AUTOSTART_STATUS_FAILURE_MESSAGE,
    });
    let renderer!: ReactTestRenderer;
    await act(async () => {
      renderer = create(createElement(SettingsView, settingsProps()));
      await Promise.resolve();
    });

    expect(renderer.root.findAllByProps({ 'aria-label': 'Launch at Login' })).toHaveLength(0);
    expect(checkAgainButton(renderer).props.children).toBe('Check again');
    expect(renderer.root.findByProps({ role: 'alert' }).props.children).toBe(
      AUTOSTART_STATUS_FAILURE_MESSAGE,
    );
    await act(async () => renderer.unmount());
  });

  it('shows Check again when registration cannot be confirmed', async () => {
    const onAutostartNotice = vi.fn();
    autostart.setAutostartEnabled.mockResolvedValue({
      status: 'failure',
      message: AUTOSTART_UPDATE_FAILURE_MESSAGE,
    });
    let renderer!: ReactTestRenderer;
    await act(async () => {
      renderer = create(createElement(SettingsView, settingsProps({ onAutostartNotice })));
      await Promise.resolve();
    });

    await act(async () => {
      launchSwitch(renderer).props.onClick();
      await Promise.resolve();
    });

    expect(renderer.root.findAllByProps({ 'aria-label': 'Launch at Login' })).toHaveLength(0);
    expect(checkAgainButton(renderer).props.children).toBe('Check again');
    expect(onAutostartNotice).toHaveBeenCalledExactlyOnceWith(AUTOSTART_UPDATE_FAILURE_MESSAGE);
    expect(renderer.root.findByProps({ role: 'alert' }).props.children).toBe(
      AUTOSTART_UPDATE_FAILURE_MESSAGE,
    );
    await act(async () => renderer.unmount());
  });

  it('keeps the launch setting non-interactive until its initial read settles', async () => {
    autostart.readAutostartEnabled.mockReturnValue(new Promise(() => {}));
    let renderer!: ReactTestRenderer;
    await act(async () => {
      renderer = create(createElement(SettingsView, settingsProps()));
    });

    expect(renderer.root.findAllByProps({ 'aria-label': 'Launch at Login' })).toHaveLength(0);
    expect(checkAgainButton(renderer).props.disabled).toBe(true);
    await act(async () => {
      checkAgainButton(renderer).props.onClick();
    });
    expect(autostart.readAutostartEnabled).toHaveBeenCalledExactlyOnceWith();
    expect(autostart.setAutostartEnabled).not.toHaveBeenCalled();
    await act(async () => renderer.unmount());
  });

  it('shows Check again without an error when preview status is unavailable', async () => {
    autostart.readAutostartEnabled.mockResolvedValue({ status: 'unavailable' });
    let renderer!: ReactTestRenderer;
    await act(async () => {
      renderer = create(createElement(SettingsView, settingsProps()));
      await Promise.resolve();
    });

    expect(checkAgainButton(renderer).props.children).toBe('Check again');
    expect(renderer.root.findAllByProps({ role: 'alert' })).toHaveLength(0);
    await act(async () => renderer.unmount());
  });

  it('re-reads unknown status without writing and restores the switch on success', async () => {
    autostart.readAutostartEnabled
      .mockResolvedValueOnce({ status: 'unavailable' })
      .mockResolvedValueOnce({ status: 'ok', enabled: true });
    let renderer!: ReactTestRenderer;
    await act(async () => {
      renderer = create(createElement(SettingsView, settingsProps()));
      await Promise.resolve();
    });

    await act(async () => {
      checkAgainButton(renderer).props.onClick();
      await Promise.resolve();
    });

    expect(autostart.readAutostartEnabled).toHaveBeenCalledTimes(2);
    expect(autostart.setAutostartEnabled).not.toHaveBeenCalled();
    expect(launchSwitch(renderer).props['aria-checked']).toBe(true);
    await act(async () => renderer.unmount());
  });

  it('ignores an initial read that resolves after unmount', async () => {
    let resolveRead!: (result: { status: 'ok'; enabled: boolean }) => void;
    autostart.readAutostartEnabled.mockReturnValue(new Promise((resolve) => {
      resolveRead = resolve;
    }));
    const consoleError = vi.spyOn(console, 'error').mockImplementation(() => undefined);
    let renderer!: ReactTestRenderer;
    await act(async () => {
      renderer = create(createElement(SettingsView, settingsProps()));
    });
    await act(async () => renderer.unmount());
    const errorsBeforeLateRead = consoleError.mock.calls.length;
    await act(async () => {
      resolveRead({ status: 'ok', enabled: true });
      await Promise.resolve();
    });

    expect(consoleError).toHaveBeenCalledTimes(errorsBeforeLateRead);
  });

  it('turns on after a confirmed login-item write', async () => {
    let renderer!: ReactTestRenderer;
    await act(async () => {
      renderer = create(createElement(SettingsView, settingsProps()));
      await Promise.resolve();
    });

    await act(async () => {
      launchSwitch(renderer).props.onClick();
      await Promise.resolve();
    });

    expect(autostart.setAutostartEnabled).toHaveBeenCalledExactlyOnceWith(true);
    expect(launchSwitch(renderer).props['aria-checked']).toBe(true);
    expect(renderer.root.findAllByProps({ role: 'alert' })).toHaveLength(0);
    await act(async () => renderer.unmount());
  });
});
