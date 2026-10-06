import { act, create, type ReactTestRenderer } from 'react-test-renderer';
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';
import { formatFooterAge, useFooterStatus } from '../src/hooks/use_footer_status';

type FooterStatus = ReturnType<typeof useFooterStatus>;

let latestStatus: FooterStatus | undefined;

function FooterStatusHarness({
  visible,
  loading,
  lastUpdatedAt,
}: {
  visible: boolean;
  loading: boolean;
  lastUpdatedAt: number | null;
}) {
  latestStatus = useFooterStatus(visible, loading, lastUpdatedAt);
  return null;
}

beforeAll(() => {
  globalThis.IS_REACT_ACT_ENVIRONMENT = true;
});

beforeEach(() => {
  vi.useFakeTimers();
  vi.setSystemTime(new Date('2026-08-26T12:00:00Z'));
  latestStatus = undefined;
});

afterEach(() => {
  vi.useRealTimers();
});

afterAll(() => {
  delete (globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT;
});

describe('useFooterStatus', () => {
  let renderer: ReactTestRenderer | undefined;

  afterEach(async () => {
    if (renderer) await act(async () => renderer?.unmount());
    renderer = undefined;
  });

  it('reports loading and not-yet-updated states', async () => {
    await act(async () => {
      renderer = create(
        <FooterStatusHarness visible loading lastUpdatedAt={null} />,
      );
    });
    expect(latestStatus).toEqual({
      footerStatus: 'Updating...',
      footerStatusTitle: 'Not updated yet',
    });
  });

  it('refreshes relative time while the popover is visible', async () => {
    const lastUpdatedAt = Date.now() - 30_000;
    await act(async () => {
      renderer = create(
        <FooterStatusHarness visible loading={false} lastUpdatedAt={lastUpdatedAt} />,
      );
    });
    expect(latestStatus?.footerStatus).toBe('now');

    await act(async () => {
      vi.advanceTimersByTime(30_000);
    });
    expect(latestStatus?.footerStatus).toBe('1m');
  });

  it('runs and cleans up the timer only while visible', async () => {
    await act(async () => {
      renderer = create(
        <FooterStatusHarness visible={false} loading={false} lastUpdatedAt={Date.now()} />,
      );
    });
    expect(vi.getTimerCount()).toBe(0);

    await act(async () => {
      renderer?.update(
        <FooterStatusHarness visible loading={false} lastUpdatedAt={Date.now()} />,
      );
    });
    expect(vi.getTimerCount()).toBe(1);

    await act(async () => {
      renderer?.update(
        <FooterStatusHarness visible={false} loading={false} lastUpdatedAt={Date.now()} />,
      );
    });
    expect(vi.getTimerCount()).toBe(0);
  });
});

describe('formatFooterAge', () => {
  const now = Date.parse('2026-10-06T12:00:00Z');

  it('uses compact relative labels before falling back to the event date format', () => {
    expect(formatFooterAge(now - 59_999, now)).toBe('now');
    expect(formatFooterAge(now - 60_000, now)).toBe('1m');
    expect(formatFooterAge(now - 59 * 60_000, now)).toBe('59m');
    expect(formatFooterAge(now - 60 * 60_000, now)).toBe('1h');
    expect(formatFooterAge(now - 23 * 60 * 60_000, now)).toBe('23h');
    expect(formatFooterAge(now - 24 * 60 * 60_000, now)).toBe('Oct 5');
  });
});
