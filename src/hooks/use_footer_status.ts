import { useEffect, useState } from 'react';
import { formatEventTime } from '../services/event_log';

export function formatFooterAge(timestamp: number, now: number = Date.now()): string {
  if (!Number.isFinite(timestamp)) return '';
  const elapsed = Math.max(0, now - timestamp);
  if (elapsed < 60_000) return 'now';
  if (elapsed < 60 * 60_000) return `${Math.floor(elapsed / 60_000)}m`;
  if (elapsed < 24 * 60 * 60_000) return `${Math.floor(elapsed / (60 * 60_000))}h`;
  return formatEventTime(new Date(timestamp).toISOString(), now);
}

export function useFooterStatus(
  windowVisible: boolean,
  activeLoading: boolean,
  lastUpdatedAt: number | null,
): { footerStatus: string; footerStatusTitle: string } {
  const [, setStatusTick] = useState(0);

  useEffect(() => {
    if (!windowVisible) return;
    const interval = setInterval(() => setStatusTick((tick) => tick + 1), 30 * 1000);
    return () => clearInterval(interval);
  }, [windowVisible]);

  return {
    footerStatus: activeLoading
      ? 'Updating...'
      : lastUpdatedAt != null
        ? formatFooterAge(lastUpdatedAt)
        : '',
    footerStatusTitle: lastUpdatedAt != null
      ? `Last updated ${new Date(lastUpdatedAt).toLocaleTimeString('en-GB', { hour: '2-digit', minute: '2-digit' })}`
      : 'Not updated yet',
  };
}
