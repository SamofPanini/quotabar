import type { CodexRateLimitWindow, UsageInfo } from '../types/models';

export const PING_MARGIN_SECONDS = 60;
export type PingWindowState = 'open' | 'closed' | 'unknown';

// Keep the accepted RFC3339 subset in lock-step with Rust. Date normalizes
// invalid fields (for example, September 31), so validate fields before it.
const RFC3339 = /^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2}):(\d{2})(?:\.\d+)?(?:Z|([+-])(\d{2}):(\d{2}))$/;

function isNonNegativeFinite(value: unknown): value is number {
  return typeof value === 'number' && Number.isFinite(value) && value >= 0;
}

export function parseRfc3339EpochSeconds(value: unknown): number | undefined {
  if (typeof value !== 'string') return undefined;
  const fields = RFC3339.exec(value);
  if (!fields) return undefined;
  const [, yearText, monthText, dayText, hourText, minuteText, secondText, , offsetHourText, offsetMinuteText] = fields;
  const year = Number(yearText);
  const month = Number(monthText);
  const day = Number(dayText);
  const hour = Number(hourText);
  const minute = Number(minuteText);
  const second = Number(secondText);
  const offsetHour = offsetHourText == null ? 0 : Number(offsetHourText);
  const offsetMinute = offsetMinuteText == null ? 0 : Number(offsetMinuteText);
  const leapYear = year % 4 === 0 && (year % 100 !== 0 || year % 400 === 0);
  const daysInMonth = [31, leapYear ? 29 : 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
  if (month < 1 || month > 12
    || day < 1 || day > daysInMonth[month - 1]
    || hour > 23 || minute > 59 || second > 59
    || offsetHour > 23 || offsetMinute > 59) return undefined;
  const milliseconds = new Date(value).getTime();
  return Number.isFinite(milliseconds) ? milliseconds / 1000 : undefined;
}

export function codexPingWindowState(
  primary: CodexRateLimitWindow | undefined,
  nowSeconds: number,
): PingWindowState {
  if (!primary
    || !isNonNegativeFinite(primary.usedPercent)
    || !isNonNegativeFinite(primary.resetsAt)
    || !isNonNegativeFinite(primary.windowMinutes)
    || primary.windowMinutes <= 0) return 'unknown';
  return primary.usedPercent > 0
    || primary.resetsAt - nowSeconds < primary.windowMinutes * 60 - PING_MARGIN_SECONDS
    ? 'open'
    : 'closed';
}

export function claudePingWindowState(
  session: UsageInfo | undefined,
  nowSeconds: number,
): PingWindowState {
  if (!session
    || !isNonNegativeFinite(session.used)
    || !isNonNegativeFinite(session.limit)
    || !isNonNegativeFinite(session.percentage)) return 'unknown';
  if (session.percentage > 0) return 'open';
  if (session.resetTime == null) return 'closed';
  const reset = parseRfc3339EpochSeconds(session.resetTime);
  if (reset == null) return 'unknown';
  return reset > nowSeconds && reset - nowSeconds < 5 * 60 * 60 - PING_MARGIN_SECONDS
    ? 'open'
    : 'unknown';
}

export function formatPingReset(epochSeconds: number | null | undefined): string {
  if (epochSeconds == null) return 'unknown time';
  const date = new Date(epochSeconds * 1000);
  if (Number.isNaN(date.getTime())) return 'unknown time';
  return date.toLocaleTimeString('en-US', { hour: 'numeric', minute: '2-digit' });
}
