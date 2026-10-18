import { useEffect, useMemo, useRef, useState } from 'react';
import { backend } from '../services/backend';
import { getBudgetForSources, getSavedMonthlyBudgets } from '../services/budget';
import type { CostDailyPoint, CostDailySeries, CostOverview, CostRangeSummary, CostSource } from '../types/models';
import { getProgressStyle } from '../utils/quota_format';
import { useLatestRequestGeneration } from '../hooks/use_latest_request_generation';
import { getCursorOnlineCostEnabled } from '../services/storage';

interface CostSummarySectionProps {
  source: CostSource | readonly CostSource[];
  refreshKey?: number;
  autoRefreshIntervalMs?: number;
  showTrend?: boolean;
}

const DEFAULT_AUTO_REFRESH_INTERVAL_MS = 5 * 60 * 1000;
const DAILY_SERIES_DAYS = 30;
export const ALL_COST_DISPLAY_NAME = 'Claude, Codex, Cursor';

export type SparkRange = '7d' | '30d';
export type MergedDailyPoints = CostDailyPoint[] & { stale: boolean };

export function mergeDailySeries(seriesList: CostDailySeries[]): MergedDailyPoints {
  const byDate = new Map<string, CostDailyPoint>();
  for (const series of seriesList) {
    for (const day of series.days) {
      const existing = byDate.get(day.date);
      if (!existing) {
        // A source-read failure represents unknown cost even if a stale or
        // malformed payload carries a zero amount. Once ranges are merged,
        // known amounts from other sources remain available below.
        byDate.set(day.date, day.sourceReadFailed
          ? { ...day, cost: null, costUsd: null, costIsLowerBound: false }
          : { ...day });
        continue;
      }
      const existingHasUnpricedUsage = hasUnpricedUsage(existing);
      const dayHasUnpricedUsage = hasUnpricedUsage(day);
      existing.cost = sumNullable([existing.cost, day.sourceReadFailed ? null : day.cost]);
      existing.costUsd = sumNullable([existing.costUsd, day.sourceReadFailed ? null : day.costUsd]);
      existing.totalTokens += day.totalTokens;
      const sourceReadFailed = Boolean(existing.sourceReadFailed || day.sourceReadFailed);
      // A partial total is a lower bound; an entirely unreadable date has no
      // displayable amount and therefore must not carry a lower-bound marker.
      existing.costIsLowerBound = dailyValue(existing) != null && Boolean(
        existing.costIsLowerBound
        || day.costIsLowerBound
        || sourceReadFailed
        || existingHasUnpricedUsage
        || dayHasUnpricedUsage,
      );
      existing.sourceReadFailed = sourceReadFailed;
    }
  }
  const days = Array.from(byDate.values()).sort((left, right) => left.date.localeCompare(right.date));
  return Object.assign(days, { stale: seriesList.some((series) => series.stale) });
}

export function sliceSparkDays(days: CostDailyPoint[], range: SparkRange): CostDailyPoint[] {
  return range === '7d' ? days.slice(-7) : days.slice(-30);
}

export function sumDailyCost(days: CostDailyPoint[]): number | null {
  return sumNullable(days.map(dailyValue));
}

function dailyValue(day: CostDailyPoint): number | null {
  return day.costUsd ?? day.cost ?? null;
}

function hasUnpricedUsage(day: CostDailyPoint): boolean {
  return day.totalTokens > 0 && dailyValue(day) == null;
}

function isUnavailableDay(day: CostDailyPoint): boolean {
  return dailyValue(day) == null && (Boolean(day.sourceReadFailed) || hasUnpricedUsage(day));
}

export function summarizeDailyCost(days: CostDailyPoint[]) {
  const cost = sumDailyCost(days);
  const unavailableDays = days.filter(isUnavailableDay);
  return {
    cost,
    costIsLowerBound: cost != null && (
      days.some((day) => Boolean(day.costIsLowerBound)) || unavailableDays.length > 0
    ),
    unpricedDays: unavailableDays.length,
  };
}

export function startCostSummaryAutoRefresh(
  autoRefreshIntervalMs: number,
  loadCost: (force: boolean) => void | Promise<void>,
): ReturnType<typeof setInterval> | undefined {
  if (autoRefreshIntervalMs <= 0) return undefined;
  // Non-forced so the backend cache TTL governs how often local logs are
  // re-parsed; only a manual refresh bypasses it.
  return setInterval(() => {
    void loadCost(false);
  }, autoRefreshIntervalMs);
}

export function getCostSummaryErrorMessage(err: unknown): string {
  if (err instanceof Error) return err.message;
  if (typeof err === 'string' && err.trim()) return err;
  if (err && typeof err === 'object' && 'message' in err) {
    const message = err.message;
    if (typeof message === 'string' && message.trim()) return message;
  }
  return 'Failed to load cost summary';
}

function formatMoney(value: number | null | undefined, currency: string): string {
  if (value == null || !Number.isFinite(value)) return 'n/a';

  const maximumFractionDigits = Math.abs(value) < 1 ? 4 : 2;
  try {
    return new Intl.NumberFormat('en-US', {
      style: 'currency',
      currency,
      minimumFractionDigits: 2,
      maximumFractionDigits,
    }).format(value);
  } catch {
    return `${currency} ${value.toFixed(maximumFractionDigits)}`;
  }
}

export function formatRangeMoney(
  value: number | null | undefined,
  currency: string,
  costIsLowerBound = false,
): string {
  const formatted = formatMoney(value, currency);
  return costIsLowerBound && formatted !== 'n/a' ? `≥ ${formatted}` : formatted;
}

function formatCompactNumber(value: number): string {
  if (!Number.isFinite(value)) return '0';
  return new Intl.NumberFormat('en-US', {
    notation: 'compact',
    maximumFractionDigits: 1,
  }).format(value);
}

function formatUpdatedAt(value: string): string {
  try {
    return new Date(value).toLocaleTimeString('en-US', {
      hour: '2-digit',
      minute: '2-digit',
    });
  } catch {
    return '';
  }
}

function formatCostNote(range: CostRangeSummary | null): string {
  if (!range) return '';
  return `${formatCompactNumber(range.tokens.totalTokens)} tokens`;
}

function pickPrimaryRange(overview: CostOverview | null): CostRangeSummary | null {
  if (!overview) return null;
  return (
    overview.ranges.find((range) => range.range === 'today' && range.validEntries > 0) ??
    overview.ranges.find((range) => range.validEntries > 0) ??
    overview.ranges[0] ??
    null
  );
}

function emptyTokens() {
  return {
    inputTokens: 0,
    outputTokens: 0,
    reasoningTokens: 0,
    cacheCreationTokens: 0,
    cacheReadTokens: 0,
    totalTokens: 0,
  };
}

function addTokens(left: ReturnType<typeof emptyTokens>, right: CostRangeSummary['tokens']) {
  left.inputTokens += right.inputTokens;
  left.outputTokens += right.outputTokens;
  left.reasoningTokens += right.reasoningTokens;
  left.cacheCreationTokens += right.cacheCreationTokens;
  left.cacheReadTokens += right.cacheReadTokens;
  left.totalTokens += right.totalTokens;
}

function sumNullable(values: Array<number | null | undefined>): number | null {
  let total = 0;
  let hasValue = false;
  for (const value of values) {
    if (value == null || !Number.isFinite(value)) continue;
    total += value;
    hasValue = true;
  }
  return hasValue ? total : null;
}

function latestTimestamp(values: string[]): string {
  return values.reduce((latest, value) => {
    const latestTime = Date.parse(latest);
    const valueTime = Date.parse(value);
    return Number.isFinite(valueTime) && valueTime > latestTime ? value : latest;
  });
}

export function mergeCostKinds(costKinds: Array<string | undefined>): string {
  // A source without usage reports 'none'; it says nothing about how the others were priced.
  const kinds = new Set(costKinds.filter((kind): kind is string => Boolean(kind) && kind !== 'none'));
  if (kinds.size === 0) return 'none';
  if (kinds.size === 1) return kinds.values().next().value ?? 'none';
  return 'mixed';
}

export function mergeCostOverviews(overviews: CostOverview[]): CostOverview {
  if (overviews.length === 1) return overviews[0];

  const rangeOrder = overviews[0]?.ranges.map((range) => range.range) ?? [];
  const ranges = rangeOrder.map((rangeName) => {
    const matching = overviews
      .map((overview) => overview.ranges.find((range) => range.range === rangeName))
      .filter((range): range is CostRangeSummary => Boolean(range));
    const first = matching[0];
    const tokens = emptyTokens();
    const modelMap = new Map<string, CostRangeSummary['models'][number]>();

    for (const range of matching) {
      addTokens(tokens, range.tokens);
      for (const model of range.models) {
        const existing = modelMap.get(model.model);
        if (!existing) {
          modelMap.set(model.model, { ...model, tokens: { ...model.tokens } });
          continue;
        }
        existing.cost = sumNullable([existing.cost, model.cost]);
        existing.costUsd = sumNullable([existing.costUsd, model.costUsd]);
        addTokens(existing.tokens, model.tokens);
      }
    }

    const sourceReadFailed = matching.some((range) => Boolean(range.sourceReadFailed));
    const cost = sumNullable(matching.map((range) => range.costUsd ?? range.cost));
    const costIsLowerBound = matching.some((range) => Boolean(range.costIsLowerBound) || (
      range.tokens.totalTokens > 0 && range.cost == null && range.costUsd == null
    )) || (sourceReadFailed && cost != null);

    return {
      ...first,
      currency: 'USD',
      cost,
      costUsd: cost,
      estimatedCost: sumNullable(matching.map((range) => range.estimatedCost)),
      estimatedCostUsd: sumNullable(matching.map((range) => range.estimatedCostUsd)),
      costKind: mergeCostKinds(matching.map((range) => range.costKind)),
      costIsLowerBound,
      sourceReadFailed,
      unpricedModels: matching.reduce((total, range) => total + (range.unpricedModels ?? 0), 0),
      tokens,
      models: Array.from(modelMap.values()).sort((left, right) => (right.costUsd ?? right.cost ?? 0) - (left.costUsd ?? left.cost ?? 0)),
      validEntries: matching.reduce((total, range) => total + range.validEntries, 0),
      skippedEntries: matching.reduce((total, range) => total + range.skippedEntries, 0),
      parseErrorEntries: matching.reduce((total, range) => total + (range.parseErrorEntries ?? 0), 0),
      elapsedMs: matching.reduce((total, range) => total + range.elapsedMs, 0),
    };
  });

  return {
    source: 'all',
    displayName: ALL_COST_DISPLAY_NAME,
    currency: 'USD',
    generatedAt: latestTimestamp(overviews.map((overview) => overview.generatedAt)),
    cached: overviews.every((overview) => overview.cached),
    stale: overviews.some((overview) => Boolean(overview.stale)),
    ranges,
  };
}

export function costFooterLabels(
  range: CostRangeSummary | null,
  overview: CostOverview,
  dailyStale = false,
): string[] {
  const labels = [range?.label ?? overview.displayName];
  const unpricedModels = range?.unpricedModels ?? 0;
  const parseErrorEntries = range?.parseErrorEntries ?? 0;
  if (unpricedModels > 0) labels.push(`${unpricedModels} model${unpricedModels === 1 ? '' : 's'} unpriced`);
  if (parseErrorEntries > 0) labels.push(`${parseErrorEntries} parse error${parseErrorEntries === 1 ? '' : 's'}`);
  if (range?.sourceReadFailed) labels.push('usage files unreadable');
  if (range?.costKind === 'estimated_proxy' || range?.costKind === 'mixed') labels.push('Estimated');
  // The trend may come from a stale daily snapshot even when the overview is fresh.
  if (overview.stale || dailyStale) labels.push('Stale');
  else if (overview.cached) labels.push('Cached');
  return labels;
}

export default function CostSummarySection({
  source,
  refreshKey = 0,
  autoRefreshIntervalMs = DEFAULT_AUTO_REFRESH_INTERVAL_MS,
  showTrend = true,
}: CostSummarySectionProps) {
  const [overview, setOverview] = useState<CostOverview | null>(null);
  const [daily, setDaily] = useState<MergedDailyPoints | null>(null);
  const [sparkRange, setSparkRange] = useState<SparkRange>('7d');
  const [hoveredDay, setHoveredDay] = useState<CostDailyPoint | null>(null);
  const [focusedDay, setFocusedDay] = useState<CostDailyPoint | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const sourceKey = Array.isArray(source) ? source.join(',') : source;
  const allowCursorOnline = getCursorOnlineCostEnabled();
  const requestedSources = Array.isArray(source) ? source : [source];
  const sources = requestedSources.filter((item) => item !== 'cursor' || allowCursorOnline);
  const cursorCostIsOff = requestedSources.includes('cursor') && !allowCursorOnline;
  const overview_generation = useLatestRequestGeneration();
  const daily_generation = useLatestRequestGeneration();
  const lastRefreshKeyRef = useRef(refreshKey);

  useEffect(() => {
    let interval: number | undefined;

    const loadCost = async (force: boolean) => {
      const generation = overview_generation.begin();
      try {
        setLoading(true);
        setError(null);
        if (sources.length === 0) {
          setOverview(null);
          return;
        }
        const overviews = await Promise.all(
          sources.map((item) => backend.getCostOverview(item, force, allowCursorOnline)),
        );
        if (!overview_generation.isCurrent(generation)) return;
        const data = mergeCostOverviews(overviews);
        setOverview(data);
      } catch (err) {
        if (!overview_generation.isCurrent(generation)) return;
        setError(getCostSummaryErrorMessage(err));
      } finally {
        if (overview_generation.isCurrent(generation)) {
          setLoading(false);
        }
      }
    };

    const loadDaily = async (force: boolean) => {
      const generation = daily_generation.begin();
      try {
        if (sources.length === 0) {
          setDaily(null);
          return;
        }
        const seriesList = await Promise.all(
          sources.map((item) => backend.getCostDaily(item, DAILY_SERIES_DAYS, force, allowCursorOnline)),
        );
        if (!daily_generation.isCurrent(generation)) return;
        setDaily(mergeDailySeries(seriesList));
      } catch (err) {
        if (!daily_generation.isCurrent(generation)) return;
        // The trend falls back to per-model bars; surface why in the console.
        console.error('Failed to load daily cost series:', err);
        setDaily(null);
      }
    };

    // Force only when the manual-refresh nonce actually advanced; otherwise a
    // single manual refresh would make every later effect run bypass the cache.
    const manualRefresh = refreshKey > lastRefreshKeyRef.current;
    lastRefreshKeyRef.current = refreshKey;
    loadCost(manualRefresh);
    void loadDaily(manualRefresh);
    interval = startCostSummaryAutoRefresh(autoRefreshIntervalMs, (force) => {
      void loadCost(force);
      void loadDaily(force);
    });

    return () => {
      overview_generation.invalidate();
      daily_generation.invalidate();
      if (interval !== undefined) {
        clearInterval(interval);
      }
    };
  }, [sourceKey, allowCursorOnline, refreshKey, autoRefreshIntervalMs, overview_generation, daily_generation]);

  const primaryRange = useMemo(() => pickPrimaryRange(overview), [overview]);
  const topModels = primaryRange?.models.slice(0, 3) ?? [];

  // Budgets are edited in Settings, which unmounts this component, so a
  // read-on-mount snapshot stays in sync.
  const monthlyBudget = useMemo(() => {
    return getBudgetForSources(getSavedMonthlyBudgets(), sources);
  }, [sourceKey, allowCursorOnline]);
  const monthRange = overview?.ranges.find((range) => range.range === 'month') ?? null;
  const monthCost = monthRange ? monthRange.costUsd ?? monthRange.cost : null;
  const budgetPercent = monthlyBudget != null && monthCost != null
    ? (monthCost / monthlyBudget) * 100
    : null;

  return (
    <div className="section cost-section">
      <div className="cost-title-row">
        <span className="section-title">API-equivalent usage</span>
        <span className="cost-title-meta">
          <span className="cost-estimate-badge">Local estimate</span>
          {overview && <span className="cost-note">{formatCostNote(primaryRange)}</span>}
        </span>
      </div>

      {cursorCostIsOff && (
        <div className="cost-inline-error compact">Cursor online cost is off. Turn it on in Settings.</div>
      )}

      {loading && !overview && (
        <div className="cost-loading">Loading cost...</div>
      )}

      {error && !overview && (
        <div className="cost-inline-error">{error}</div>
      )}

      {overview && (
        <div className="cost-panel">
          <div className="cost-range-grid">
            {overview.ranges.map((range) => {
              const displayedCost = range.cost;
              const showsLowerBound = range.costIsLowerBound && formatMoney(displayedCost, range.currency) !== 'n/a';
              return (
                <div
                  className={`cost-range ${range.range === primaryRange?.range ? 'active' : ''}`}
                  key={range.range}
                >
                  <span className="cost-range-label">{range.label}</span>
                  <strong
                    className={showsLowerBound ? 'cost-range-value warning' : 'cost-range-value'}
                    title={showsLowerBound ? 'Cost is at least the displayed amount' : undefined}
                    aria-label={showsLowerBound ? `Cost is at least ${formatMoney(displayedCost, range.currency)}` : undefined}
                  >
                    {formatRangeMoney(displayedCost, range.currency, showsLowerBound)}
                  </strong>
                </div>
              );
            })}
          </div>

          {primaryRange && (
            <>
              {budgetPercent != null && monthlyBudget != null && (
                <div className="budget-panel">
                  <div className="budget-row">
                    <span>Monthly budget</span>
                    <strong>
                      {formatRangeMoney(monthCost, monthRange?.currency ?? 'USD', monthRange?.costIsLowerBound)}
                      {' / '}
                      {formatMoney(monthlyBudget, monthRange?.currency ?? 'USD')}
                    </strong>
                  </div>
                  <div
                    className="budget-track"
                    role="progressbar"
                    aria-label="Monthly API-equivalent budget used"
                    aria-valuemin={0}
                    aria-valuemax={100}
                    aria-valuenow={Math.min(100, Math.round(budgetPercent))}
                    aria-valuetext={`${Math.round(budgetPercent)}% of monthly budget used`}
                  >
                    <div className="budget-fill" style={getProgressStyle(budgetPercent)} />
                  </div>
                </div>
              )}

              {showTrend && daily && daily.length > 0 ? (
                (() => {
                  const sparkDays = sliceSparkDays(daily, sparkRange);
                  const maxCost = Math.max(...sparkDays.flatMap((day) => {
                    const value = dailyValue(day);
                    return value == null ? [] : [value];
                  }), 0);
                  const focusedIndex = focusedDay
                    ? sparkDays.findIndex((day) => day.date === focusedDay.date)
                    : -1;
                  const inspectedCandidate = hoveredDay ?? focusedDay;
                  const inspectedIndex = inspectedCandidate
                    ? sparkDays.findIndex((day) => day.date === inspectedCandidate.date)
                    : -1;
                  const inspectedDay = inspectedIndex >= 0 ? sparkDays[inspectedIndex] : null;
                  const activeIndex = inspectedIndex >= 0 ? inspectedIndex : sparkDays.length - 1;
                  const activeDay = sparkDays[activeIndex];
                  const activeValue = activeDay ? dailyValue(activeDay) : null;
                  const focusDay = (index: number) => {
                    const day = sparkDays[index];
                    if (day) setFocusedDay(day);
                  };
                  return (
                    <>
                      <div
                        className="spark-bars"
                        role="slider"
                        tabIndex={0}
                        aria-label="Daily API-equivalent usage trend"
                        aria-valuemin={1}
                        aria-valuemax={sparkDays.length}
                        aria-valuenow={activeIndex + 1}
                        aria-valuetext={`${activeDay.date} · ${formatRangeMoney(activeValue, primaryRange.currency, activeDay.costIsLowerBound)}`}
                        onFocus={() => focusDay(activeIndex)}
                        onBlur={() => setFocusedDay(null)}
                        onMouseLeave={() => setHoveredDay(null)}
                        onKeyDown={(event) => {
                          const keyboardIndex = focusedIndex >= 0 ? focusedIndex : activeIndex;
                          let nextIndex = keyboardIndex;
                          if (event.key === 'ArrowLeft' || event.key === 'ArrowDown') nextIndex = Math.max(0, keyboardIndex - 1);
                          else if (event.key === 'ArrowRight' || event.key === 'ArrowUp') nextIndex = Math.min(sparkDays.length - 1, keyboardIndex + 1);
                          else if (event.key === 'Home') nextIndex = 0;
                          else if (event.key === 'End') nextIndex = sparkDays.length - 1;
                          else return;
                          event.preventDefault();
                          setHoveredDay(null);
                          focusDay(nextIndex);
                        }}
                      >
                        {sparkDays.map((day, index) => {
                          const value = dailyValue(day);
                          const gap = isUnavailableDay(day);
                          const height = maxCost > 0 && value != null ? Math.max(8, (value / maxCost) * 100) : 8;
                          const isHovered = inspectedDay?.date === day.date;
                          return (
                            <span
                              className={['spark-bar-hit', gap && 'gap', isHovered && 'hovered'].filter(Boolean).join(' ')}
                              key={day.date}
                              onMouseEnter={() => setHoveredDay(day)}
                              aria-hidden="true"
                            >
                              {!gap && (
                                <span
                                  className={`spark-bar ${index === sparkDays.length - 1 ? 'latest' : ''} ${isHovered ? 'hovered' : ''}`}
                                  style={{ height: `${height}%` }}
                                />
                              )}
                            </span>
                          );
                        })}
                      </div>
                      <div className="cost-footer">
                        <span className={inspectedDay ? 'spark-hover-label' : undefined}>
                          {inspectedDay
                            ? `${inspectedDay.date} · ${formatRangeMoney(dailyValue(inspectedDay), primaryRange.currency, inspectedDay.costIsLowerBound)}`
                            : (() => {
                              const total = summarizeDailyCost(sparkDays);
                              const naDays = total.unpricedDays;
                              return `${sparkRange === '7d' ? 'Past 7 days' : 'Past 30 days'} · ${formatRangeMoney(total.cost, primaryRange.currency, total.costIsLowerBound)}${naDays > 0 ? ` · ${naDays} day${naDays === 1 ? '' : 's'} n/a` : ''}`;
                            })()}
                        </span>
                        <span className="spark-range-chips">
                          <button
                            type="button"
                            className={`spark-chip ${sparkRange === '7d' ? 'active' : ''}`}
                            onClick={() => setSparkRange('7d')}
                          >
                            7D
                          </button>
                          <button
                            type="button"
                            className={`spark-chip ${sparkRange === '30d' ? 'active' : ''}`}
                            onClick={() => setSparkRange('30d')}
                          >
                            30D
                          </button>
                        </span>
                      </div>
                    </>
                  );
                })()
              ) : showTrend && topModels.length > 0 ? (
                <div className="spark-bars">
                  {topModels.map((model, index) => (
                    <div
                      className="spark-bar"
                      key={model.model}
                      title={`${model.model}: ${formatMoney(model.cost, primaryRange.currency)}`}
                      style={{ height: `${Math.max(18, 44 - index * 9)}%` }}
                    />
                  ))}
                </div>
              ) : null}
            </>
          )}

          <div className="cost-footer">
            <span className="cost-footer-details">{costFooterLabels(primaryRange, overview, Boolean(showTrend && daily?.stale)).join(' · ')}</span>
            <span>{formatUpdatedAt(overview.generatedAt)}</span>
          </div>

          {error && <div className="cost-inline-error compact">{error}</div>}
        </div>
      )}
    </div>
  );
}
