import { createElement, useRef } from 'react';
import { act, create, type ReactTestRenderer } from 'react-test-renderer';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { usePopoverWindow } from '../src/hooks/use_popover_window';

const boundary = vi.hoisted(() => ({ resizeWindow: vi.fn(), getCurrentWindow: vi.fn() }));
vi.mock('@tauri-apps/api/window', () => ({ getCurrentWindow: boundary.getCurrentWindow }));
vi.mock('../src/services/backend', () => ({ backend: { resizeWindow: boundary.resizeWindow } }));

class ResizeObserverStub {
  observe(): void {}
  disconnect(): void {}
}

function container(scrollHeight: number, panel?: { scrollHeight: number; clientHeight: number }) {
  return {
    scrollHeight,
    querySelector: (selector: string) => selector === '.panel-scroll' && panel
      ? { ...panel, children: [] }
      : null,
  } as unknown as HTMLDivElement;
}

function Probe({ element, scale }: { element: HTMLDivElement; scale: number }) {
  const ref = useRef<HTMLDivElement | null>(element);
  usePopoverWindow(ref, [], scale);
  return null;
}

let renderer: ReactTestRenderer | undefined;

async function render(element: HTMLDivElement, scale: number): Promise<void> {
  await act(async () => { renderer = create(createElement(Probe, { element, scale })); });
  await act(async () => { await vi.advanceTimersByTimeAsync(300); });
}

beforeEach(() => {
  vi.useFakeTimers();
  vi.stubGlobal('IS_REACT_ACT_ENVIRONMENT', true);
  vi.stubGlobal('ResizeObserver', ResizeObserverStub);
  vi.stubGlobal('window', { __TAURI_INTERNALS__: {} });
  boundary.resizeWindow.mockReset().mockResolvedValue(undefined);
  boundary.getCurrentWindow.mockReturnValue({
    isVisible: () => Promise.resolve(false),
    onFocusChanged: () => Promise.resolve(() => {}),
  });
});

afterEach(async () => {
  if (renderer) await act(async () => renderer?.unmount());
  renderer = undefined;
  vi.useRealTimers();
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

describe('scaled popover sizing', () => {
  it('keeps the existing 100% formula', async () => {
    await render(container(400), 1);
    expect(boundary.resizeWindow).toHaveBeenLastCalledWith(424, 340);
  });

  it('scales both dimensions at 150%', async () => {
    await render(container(400), 1.5);
    expect(boundary.resizeWindow).toHaveBeenLastCalledWith(636, 510);
  });

  it('includes internal panel overflow while hidden', async () => {
    await render(container(400, { scrollHeight: 600, clientHeight: 400 }), 1);
    expect(boundary.resizeWindow).toHaveBeenLastCalledWith(620, 340);
  });
});
