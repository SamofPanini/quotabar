import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { act, create, type ReactTestRenderer } from 'react-test-renderer';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import {
  UI_SCALE_ERROR,
  UI_SCALE_KEY,
  getSavedUiScale,
  useUiScale,
} from '../src/hooks/use_ui_scale';
import { subscribeStorageReadFailures } from '../src/services/storage';
import SettingsView from '../src/components/SettingsView';

const native = vi.hoisted(() => ({ setZoom: vi.fn() }));
vi.mock('@tauri-apps/api/webview', () => ({ getCurrentWebview: () => native }));
vi.mock('../src/services/backend', () => ({ hasTauriBackend: () => true }));

interface Deferred {
  promise: Promise<void>;
  resolve(): void;
  reject(reason: unknown): void;
}

function deferred(): Deferred {
  let resolve!: () => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<void>((done, fail) => {
    resolve = done;
    reject = fail;
  });
  return { promise, resolve, reject };
}

let renderer: ReactTestRenderer | undefined;
let state: ReturnType<typeof useUiScale>;
let store: Map<string, string>;
const onError = vi.fn();

function Probe() {
  state = useUiScale(onError);
  return null;
}

async function mount(): Promise<void> {
  await act(async () => {
    renderer = create(createElement(Probe));
    await Promise.resolve();
  });
}

beforeEach(() => {
  vi.stubGlobal('IS_REACT_ACT_ENVIRONMENT', true);
  store = new Map();
  vi.stubGlobal('localStorage', {
    getItem: (key: string) => store.get(key) ?? null,
    setItem: (key: string, value: string) => store.set(key, value),
  });
  vi.stubGlobal('window', { __TAURI_INTERNALS__: {} });
  native.setZoom.mockReset().mockResolvedValue(undefined);
  onError.mockReset();
});

afterEach(async () => {
  if (renderer) await act(async () => renderer?.unmount());
  renderer = undefined;
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

describe('interface size', () => {
  it('uses 100% for missing and invalid preferences, reporting invalid storage through the existing channel', () => {
    expect(getSavedUiScale()).toBe(1);
    const notified = vi.fn();
    const unsubscribe = subscribeStorageReadFailures(notified);
    store.set(UI_SCALE_KEY, '2');
    expect(getSavedUiScale()).toBe(1);
    store.set(UI_SCALE_KEY, 'abc');
    expect(getSavedUiScale()).toBe(1);
    expect(notified).toHaveBeenCalledTimes(2);
    unsubscribe();
  });

  it('does not call native zoom for a missing preference, but restores a saved 150% without writing', async () => {
    await mount();
    expect(state.scale).toBe(1);
    expect(native.setZoom).not.toHaveBeenCalled();
    await act(async () => renderer?.unmount());
    renderer = undefined;

    store.set(UI_SCALE_KEY, '1.5');
    await mount();
    expect(state.scale).toBe(1.5);
    expect(native.setZoom).toHaveBeenLastCalledWith(1.5);
    expect(native.setZoom).toHaveBeenCalledTimes(1);
    expect(store.get(UI_SCALE_KEY)).toBe('1.5');
  });

  it('serializes a slow older change before applying the latest change', async () => {
    await mount();
    const slow = deferred();
    native.setZoom.mockImplementationOnce(() => slow.promise);
    let first!: Promise<void>;
    let second!: Promise<void>;
    await act(async () => { first = state.changeScale(1.25); });
    await act(async () => { second = state.changeScale(1.5); });
    expect(native.setZoom.mock.calls.map(([value]) => value)).toEqual([1.25]);
    await act(async () => { slow.resolve(); await first; await second; });
    expect(native.setZoom.mock.calls.map(([value]) => value)).toEqual([1.25, 1.5]);
    expect(state.scale).toBe(1.5);
    expect(store.get(UI_SCALE_KEY)).toBe('1.5');
  });

  it('restores the confirmed scale when an older success is followed by a latest failure', async () => {
    await mount();
    const slow = deferred();
    native.setZoom.mockImplementationOnce(() => slow.promise).mockRejectedValueOnce(new Error('zoom failed'));
    let first!: Promise<void>;
    let second!: Promise<void>;
    await act(async () => { first = state.changeScale(1.25); });
    await act(async () => { second = state.changeScale(1.5); });
    await act(async () => { slow.resolve(); await first; await second; });
    expect(native.setZoom.mock.calls.map(([value]) => value)).toEqual([1.25, 1.5, 1]);
    expect(state.scale).toBe(1);
    expect(store.has(UI_SCALE_KEY)).toBe(false);
    expect(onError).toHaveBeenCalledTimes(1);
    expect(onError).toHaveBeenCalledWith(UI_SCALE_ERROR);
  });

  it('skips an expired middle request in a three-click sequence', async () => {
    await mount();
    const slow = deferred();
    native.setZoom.mockImplementationOnce(() => slow.promise);
    let first!: Promise<void>;
    let last!: Promise<void>;
    await act(async () => { first = state.changeScale(1.25); });
    await act(async () => { void state.changeScale(1.5); });
    await act(async () => { last = state.changeScale(1.25); });
    await act(async () => { slow.resolve(); await first; await last; });
    expect(native.setZoom.mock.calls.map(([value]) => value)).toEqual([1.25, 1.25]);
    expect(state.scale).toBe(1.25);
  });

  it('keeps the prior scale and storage when one request rejects, then restores native zoom', async () => {
    await mount();
    native.setZoom.mockRejectedValueOnce(new Error('zoom failed'));
    await act(async () => { await state.changeScale(1.5); });
    expect(state.scale).toBe(1);
    expect(store.has(UI_SCALE_KEY)).toBe(false);
    expect(onError).toHaveBeenCalledWith(UI_SCALE_ERROR);
    expect(native.setZoom.mock.calls.map(([value]) => value)).toEqual([1.5, 1]);
  });

  it('skips pending queued work after unmount', async () => {
    await mount();
    const slow = deferred();
    native.setZoom.mockImplementationOnce(() => slow.promise);
    let first!: Promise<void>;
    await act(async () => { first = state.changeScale(1.25); });
    await act(async () => { void state.changeScale(1.5); });
    await act(async () => renderer?.unmount());
    renderer = undefined;
    await act(async () => { slow.resolve(); await first; });
    expect(native.setZoom.mock.calls.map(([value]) => value)).toEqual([1.25]);
    expect(store.has(UI_SCALE_KEY)).toBe(false);
  });

  it('renders the three settings buttons and sends 125% on click', async () => {
    const onUiScaleChange = vi.fn();
    const props = {
      isMacOS: true,
      theme: 'light',
      dockHidden: false,
      trayEntries: [],
      panelSections: { timeline: true, cost: true, trend: true, tips: true },
      trayStyle: 'percent',
      trayCycle: false,
      menuBarQuotaWindow: 'weekly',
      claudeMenuBarQuotaWindow: 'weekly',
      events: [],
      notificationSettings: { q80: true, q95: true, q100: true, bonusReady: true, bonus: false },
      switcherVisibility: { claude: true, codex: true, cursor: true, grok: true, antigravity: true },
      uiScale: 1.25,
      onClose: () => {}, onThemeChange: () => {}, onDockToggle: () => {}, onTrayToggle: () => {},
      onPanelSectionToggle: () => {}, onTrayStyleChange: () => {}, onTrayCycleToggle: () => {},
      onMenuBarQuotaWindowChange: () => {}, onClaudeMenuBarQuotaWindowChange: () => {},
      onNotificationToggle: () => {}, onSwitcherToggle: () => {}, onUiScaleChange,
      onApplyPreset: () => {}, onSelectEventProvider: () => {},
    };
    const html = renderToStaticMarkup(createElement(SettingsView, props));
    expect(html).toContain('aria-label="Interface size"');
    expect(html).toContain('>100%</button>');
    expect(html).toContain('>125%</button>');
    expect(html).toContain('>150%</button>');
    let settingsRenderer!: ReactTestRenderer;
    await act(async () => {
      settingsRenderer = create(createElement(SettingsView, props));
      await Promise.resolve();
    });
    const buttons = settingsRenderer.root.findByProps({ 'aria-label': 'Interface size' }).findAllByType('button');
    expect(buttons.map((button) => button.props['aria-pressed'])).toEqual([false, true, false]);
    await act(async () => buttons[1]?.props.onClick());
    expect(onUiScaleChange).toHaveBeenCalledWith(1.25);
    await act(async () => settingsRenderer.unmount());
  });
});
