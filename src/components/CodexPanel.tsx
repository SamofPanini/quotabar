import { useEffect, useState, useCallback, useRef, type CSSProperties } from 'react';
import { backend } from '../services/backend';
import CostSummarySection from './CostSummarySection';
import ProviderDetailHeader from './ProviderDetailHeader';
import ResetTimeline from './ResetTimeline';
import SmartTip from './SmartTip';
import type {
  CodexData,
  CodexProfileQuota,
  CodexProfilesResponse,
  CodexRateLimitWindow,
  CodexRateLimits,
  CodexResetCredit,
  CodexResetCredits,
  CodexWeeklyQuota,
  CodexWeeklyValueEstimate,
} from '../types/models';
import {
  buildCodexQuotaWindows,
  sortMostConstrained,
  type CodexTrayAccountSnapshot,
  type QuotaWindowSummary,
} from '../services/provider_summary';
import {
  checkWeeklyQuotaWindow,
  checkWeeklyValueEstimate,
  formatLocalExtrasPaused,
  formatOfficialUpdatedAt,
  isHardDisplayCheck,
  isSoftDisplayCheck,
  isWeeklyExhausted,
} from '../services/codex_weekly_display';
import { getAvailableResetCredits, getHighUsageTip } from '../services/detail_helpers';
import { clampProgressValue, formatPaceText, formatPlanType, formatResetTime, getProgressStyle } from '../utils/quota_format';
import { defaultPanelSections, type PanelSectionVisibility } from '../services/panel_sections';
import { useLatestRequestGeneration } from '../hooks/use_latest_request_generation';

interface CodexPanelProps {
  onConnectionChange?: (connected: boolean) => void;
  onUsageChange?: (usedPercent: number | null) => void;
  autoRefreshIntervalMs?: number;
  manualRefreshNonce?: number;
  onLoadingChange?: (loading: boolean) => void;
  onQuotaWindowsChange?: (windows: QuotaWindowSummary[]) => void;
  onTrayQuotaSnapshotsChange?: (snapshots: CodexTrayAccountSnapshot[]) => void;
  showCostSummary?: boolean;
  sections?: PanelSectionVisibility;
  onBonusExpiring?: (daysLeft: number) => void;
  onBonusReadyChange?: (ready: { exhausted: boolean; availableCount: number }) => void;
  onOpenDashboard?: () => void;
  onPingContextChange?: (context: CodexPingContext) => void;
}

export interface CodexPingContext {
  alias: string;
  limits: CodexRateLimits | null;
  available: boolean;
  unavailableReason?: string;
}

interface PendingTrayCoordination {
  generation: number;
  defaultSnapshot?: CodexTrayAccountSnapshot;
  profileSnapshots?: CodexTrayAccountSnapshot[];
}

function formatSubscriptionDate(dateStr?: string): string {
  if (!dateStr) return 'Unknown';
  try {
    const date = new Date(dateStr);
    return date.toLocaleDateString('en-US', {
      year: 'numeric',
      month: 'short',
      day: 'numeric',
    });
  } catch {
    return dateStr;
  }
}

function formatCodexPlan(planType?: string): string {
  return `ChatGPT ${formatPlanType(planType, 'Pro')}`;
}

function formatWindowLabel(minutes?: number, kind: 'primary' | 'secondary' = 'primary'): string {
  if (!minutes) return 'Limit';
  if (minutes >= 1440) {
    const days = Math.round(minutes / 1440);
    if (days === 7) return kind === 'secondary' ? 'Weekly limit' : '7-day window';
    return `${days}d ${kind === 'secondary' ? 'limit' : 'window'}`;
  }
  if (minutes >= 60) {
    const hours = Math.round(minutes / 60);
    return `${hours}-hour window`;
  }
  return `${minutes}m`;
}

function formatResetAt(value?: number): string {
  if (!value) return '';
  const date = new Date(value * 1000);
  if (Number.isNaN(date.getTime())) return '';
  const now = new Date();
  const sameDay = date.toDateString() === now.toDateString();
  const time = date.toLocaleTimeString('en-US', {
    hour: 'numeric',
    minute: '2-digit',
  });
  if (sameDay) return `Today, ${time}`;
  const day = date.toLocaleDateString('en-US', {
    weekday: 'short',
    month: 'short',
    day: 'numeric',
  });
  return `${day}, ${time}`;
}

function formatGrantDate(value?: string): string {
  if (!value) return 'Unknown';
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return value;
  return date.toLocaleDateString('en-US', {
    month: 'short',
    day: 'numeric',
  });
}

function formatCreditBalance(balance?: string): string {
  if (balance == null || balance.trim() === '') return 'n/a';
  if (!/^\d+(?:\.\d+)?$/.test(balance)) return 'n/a';
  const value = Number(balance);
  if (!Number.isFinite(value) || value < 0) return 'n/a';
  return new Intl.NumberFormat('en-US', { maximumFractionDigits: 2 }).format(value);
}

const USD_FORMAT = new Intl.NumberFormat('en-US', {
  style: 'currency',
  currency: 'USD',
  minimumFractionDigits: 2,
  maximumFractionDigits: 2,
});

const COMPACT_TOKEN_FORMAT = new Intl.NumberFormat('en-US', {
  notation: 'compact',
  maximumFractionDigits: 1,
});


function selectOfficialWeeklyWindow(
  limits: CodexRateLimits | null,
  quota: CodexWeeklyQuota | null,
): CodexRateLimitWindow | undefined {
  const candidates = [limits?.secondary, limits?.primary].filter(
    (window): window is CodexRateLimitWindow => window != null,
  );
  if (quota) {
    const exact = candidates.find((window) => window.windowMinutes === quota.windowMinutes);
    if (exact) return exact;
  }
  return candidates.find((window) => window.windowMinutes === 10_080)
    ?? limits?.secondary
    ?? limits?.primary;
}

function selectOfficialWeeklyLimitWindow(
  limits: CodexRateLimits | null,
): CodexRateLimitWindow | undefined {
  return [limits?.secondary, limits?.primary].find(
    (window): window is CodexRateLimitWindow => window?.windowMinutes === 10_080,
  );
}

const BONUS_EXPIRY_REMINDER_DAYS = 3;

function getDaysLeft(value?: string): number | null {
  if (!value) return null;
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return null;
  return Math.max(0, Math.ceil((date.getTime() - Date.now()) / 86_400_000));
}

interface BonusGrantGroup {
  key: string;
  count: number;
  grantedAt?: string;
  expiresAt?: string;
}

function buildBonusGrantGroups(credits: CodexResetCredit[]): BonusGrantGroup[] {
  const groups = new Map<string, BonusGrantGroup>();
  for (const credit of credits) {
    const key = `${credit.grantedAt ?? 'unknown'}-${credit.expiresAt ?? 'unknown'}`;
    const existing = groups.get(key);
    if (existing) {
      existing.count += 1;
      continue;
    }
    groups.set(key, {
      key,
      count: 1,
      grantedAt: credit.grantedAt,
      expiresAt: credit.expiresAt,
    });
  }
  return Array.from(groups.values());
}

function getTrayUsedPercent(limits: CodexRateLimits): number | null {
  if (limits.secondary?.usedPercent != null) {
    return limits.secondary.usedPercent;
  }
  if (limits.primary?.usedPercent != null) {
    return limits.primary.usedPercent;
  }
  return null;
}

function profileTraySnapshot(profile: CodexProfileQuota): CodexTrayAccountSnapshot {
  const hasWindows = Boolean(profile.primary || profile.secondary);
  const freshness = profile.status === 'connected' && !profile.error
    ? 'fresh'
    : (profile.status === 'stale' || (Boolean(profile.error) && hasWindows))
      ? 'last-good-stale'
      : 'unavailable';
  return {
    accountId: profile.alias,
    connected: profile.status === 'connected' || profile.status === 'stale',
    freshness,
    primary: profile.primary,
    secondary: profile.secondary,
    ordinaryUsageAllowed: profile.ordinaryUsageAllowed,
  };
}

function staleProfile(profile: CodexProfileQuota): CodexProfileQuota {
  return {
    ...profile,
    status: 'stale',
    error: 'Custom profile registry unavailable',
  };
}

function customProfileStatus(profile: CodexProfileQuota): {
  label: string;
  tone: 'online' | 'pending' | 'offline' | 'error';
} {
  switch (profile.status) {
    case 'connected':
      return { label: 'Connected', tone: 'online' };
    case 'stale':
      return { label: 'Stale data', tone: 'pending' };
    case 'offline':
      return { label: 'Offline', tone: 'offline' };
    default:
      return { label: 'Quota unavailable', tone: 'error' };
  }
}

const CUSTOM_PROFILE_DIAGNOSTICS: Record<string, string> = {
  invalid_row: 'This profile entry is invalid.',
  invalid_alias: 'This profile alias is invalid or duplicated.',
  invalid_home_path: 'This profile home must be an absolute path without .. components.',
  invalid_home: 'This profile home is unavailable or is not a directory.',
  default_home_conflict: 'This profile home is already the Default account.',
  duplicate_home: 'Another custom profile already uses this home.',
};

function customProfileDiagnostic(profile: CodexProfileQuota): string {
  return profile.diagnosticCode
    ? CUSTOM_PROFILE_DIAGNOSTICS[profile.diagnosticCode] ?? 'Custom Codex quota unavailable'
    : 'Custom Codex quota unavailable';
}

function ordinaryUsageLabel(allowed?: boolean | null): string {
  if (allowed === true) return 'Ordinary usage permitted';
  if (allowed === false) return 'Ordinary usage blocked';
  return 'Availability unknown';
}

export default function CodexPanel({
  onConnectionChange,
  onUsageChange,
  autoRefreshIntervalMs = 60 * 1000,
  manualRefreshNonce = 0,
  onLoadingChange,
  onQuotaWindowsChange,
  onTrayQuotaSnapshotsChange,
  showCostSummary = true,
  sections = defaultPanelSections(),
  onBonusExpiring,
  onOpenDashboard,
  onPingContextChange,
}: CodexPanelProps) {
  const [codexData, setCodexData] = useState<CodexData | null>(null);
  const [rateLimits, setRateLimits] = useState<CodexRateLimits | null>(null);
  const [resetCredits, setResetCredits] = useState<CodexResetCredits | null>(null);
  const [weeklyQuota, setWeeklyQuota] = useState<CodexWeeklyQuota | null>(null);
  const [weeklyQuotaError, setWeeklyQuotaError] = useState<string | null>(null);
  const [weeklyValueEstimate, setWeeklyValueEstimate] = useState<CodexWeeklyValueEstimate | null>(null);
  const [weeklyValueEstimateError, setWeeklyValueEstimateError] = useState<string | null>(null);
  const [officialUpdatedAt, setOfficialUpdatedAt] = useState<number | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [rateLimitsError, setRateLimitsError] = useState<string | null>(null);
  const [accountInfoError, setAccountInfoError] = useState<string | null>(null);
  const [resetCreditsError, setResetCreditsError] = useState<string | null>(null);
  const [customProfiles, setCustomProfiles] = useState<CodexProfileQuota[]>([]);
  const [registryError, setRegistryError] = useState<string | null>(null);
  const [registryProvenance, setRegistryProvenance] = useState<CodexProfilesResponse['registryProvenance'] | null>(null);
  const [selectedAccountId, setSelectedAccountId] = useState('default');
  const hasResolvedData = useRef(false);
  const defaultConnection = useRef({ info: false, infoSettled: false, limits: false, limitsSucceeded: false });
  const lastGoodCustomProfiles = useRef<CodexProfileQuota[]>([]);
  const pendingTrayCoordination = useRef<PendingTrayCoordination | null>(null);
  const request_generation = useLatestRequestGeneration();
  const weekly_request_generation = useLatestRequestGeneration();

  const publishTraySnapshots = useCallback((generation: number) => {
    const pending = pendingTrayCoordination.current;
    if (
      !pending
      || pending.generation !== generation
      || !pending.defaultSnapshot
      || !pending.profileSnapshots
      || !request_generation.isCurrent(generation)
    ) return;
    onTrayQuotaSnapshotsChange?.([
      pending.defaultSnapshot,
      ...pending.profileSnapshots,
    ]);
    pendingTrayCoordination.current = null;
  }, [onTrayQuotaSnapshotsChange, request_generation]);

  const fetchWeeklyQuota = useCallback(async () => {
    const generation = weekly_request_generation.begin();
    try {
      const weekly = await backend.getCodexWeeklyQuota();
      if (!weekly_request_generation.isCurrent(generation)) return;
      setWeeklyQuota(weekly.quota ?? null);
      setWeeklyQuotaError(weekly.error ?? null);
      setWeeklyValueEstimate(weekly.valueEstimate ?? null);
      setWeeklyValueEstimateError(weekly.valueEstimateError ?? null);
    } catch (err) {
      if (!weekly_request_generation.isCurrent(generation)) return;
      setWeeklyQuota(null);
      setWeeklyQuotaError(
        err instanceof Error ? err.message : 'Failed to load local weekly pace',
      );
      setWeeklyValueEstimate(null);
      setWeeklyValueEstimateError(null);
    }
  }, [weekly_request_generation]);

  const fetchData = useCallback(async () => {
    const generation = request_generation.begin();
    pendingTrayCoordination.current = { generation };
    defaultConnection.current = { info: false, infoSettled: false, limits: false, limitsSucceeded: false };
    setLoading(true);
    setError(null);
    setRateLimitsError(null);
    setAccountInfoError(null);
    setResetCreditsError(null);
    void fetchWeeklyQuota();

    const profilesPromise = backend.getCodexProfiles()
      .then((profiles) => {
        if (!request_generation.isCurrent(generation)) return;
        lastGoodCustomProfiles.current = profiles.profiles;
        setCustomProfiles(profiles.profiles);
        setRegistryError(profiles.registryError ?? null);
        setRegistryProvenance(profiles.registryProvenance ?? 'none');
        if (pendingTrayCoordination.current?.generation !== generation) return;
        pendingTrayCoordination.current.profileSnapshots = profiles.profiles.map(profileTraySnapshot);
        publishTraySnapshots(generation);
      })
      .catch(() => {
        if (!request_generation.isCurrent(generation)) return;
        const staleProfiles = lastGoodCustomProfiles.current.map(staleProfile);
        setCustomProfiles(staleProfiles);
        setRegistryError('Custom profile registry unavailable');
        setRegistryProvenance('none');
        if (pendingTrayCoordination.current?.generation !== generation) return;
        pendingTrayCoordination.current.profileSnapshots = staleProfiles.map(profileTraySnapshot);
        publishTraySnapshots(generation);
      });

    const infoPromise = backend.getCodexInfo()
      .then((info) => {
        if (!request_generation.isCurrent(generation)) return;
        setCodexData(info);
        setAccountInfoError(info.error ?? null);
        defaultConnection.current.info = Boolean(info.connected);
        defaultConnection.current.infoSettled = true;
        if (defaultConnection.current.limitsSucceeded) {
          onConnectionChange?.(defaultConnection.current.info || defaultConnection.current.limits);
        }
      })
      .catch(() => {
        if (!request_generation.isCurrent(generation)) return;
        setAccountInfoError('Account info unavailable');
        defaultConnection.current.infoSettled = true;
        if (defaultConnection.current.limitsSucceeded) {
          onConnectionChange?.(defaultConnection.current.limits);
        }
      });
    const limitsPromise = backend.getCodexRateLimits()
      .then((limits) => {
        if (!request_generation.isCurrent(generation)) return;
        const hasWindows = Boolean(limits.primary || limits.secondary);
        const freshness = limits.error
          ? hasWindows ? 'last-good-stale' : 'unavailable'
          : 'fresh';
        setRateLimits(limits);
        hasResolvedData.current = true;
        setError(limits.error ?? null);
        setRateLimitsError(limits.error ?? null);
        if (!limits.error) setOfficialUpdatedAt(Date.now());
        onQuotaWindowsChange?.(buildCodexQuotaWindows(limits));
        defaultConnection.current.limits = limits.connected;
        defaultConnection.current.limitsSucceeded = true;
        if (defaultConnection.current.infoSettled) {
          onConnectionChange?.(defaultConnection.current.info || defaultConnection.current.limits);
        }
        onUsageChange?.(getTrayUsedPercent(limits));
        if (pendingTrayCoordination.current?.generation !== generation) return;
        pendingTrayCoordination.current.defaultSnapshot = {
          accountId: 'default',
          connected: limits.connected,
          freshness,
          primary: limits.primary,
          secondary: limits.secondary,
          ordinaryUsageAllowed: limits.ordinaryUsageAllowed,
        };
        publishTraySnapshots(generation);
      })
      .catch(() => {
        if (!request_generation.isCurrent(generation)) return;
        setError('Quota unavailable');
        setRateLimitsError('Quota unavailable');
        if (!hasResolvedData.current) {
          onConnectionChange?.(false);
          onUsageChange?.(null);
          onQuotaWindowsChange?.([]);
        }
        if (pendingTrayCoordination.current?.generation !== generation) return;
        pendingTrayCoordination.current.defaultSnapshot = {
          accountId: 'default',
          connected: false,
          freshness: 'unavailable',
        };
        publishTraySnapshots(generation);
      });
    const creditsPromise = backend.getCodexResetCredits()
      .then((credits) => {
        if (!request_generation.isCurrent(generation)) return;
        setResetCredits(credits);
        setResetCreditsError(credits.error ?? null);
      })
      .catch(() => {
        if (!request_generation.isCurrent(generation)) return;
        setResetCreditsError('Reset credits unavailable');
      });

    await Promise.all([profilesPromise, infoPromise, limitsPromise, creditsPromise]);
    if (request_generation.isCurrent(generation)) {
      setLoading(false);
    }
  }, [
    fetchWeeklyQuota,
    onConnectionChange,
    onQuotaWindowsChange,
    onTrayQuotaSnapshotsChange,
    onUsageChange,
    publishTraySnapshots,
    request_generation,
  ]);

  useEffect(() => {
    fetchData();
    // Refresh in background at configured interval; 0 pauses polling.
    if (autoRefreshIntervalMs <= 0) return;
    const interval = setInterval(fetchData, autoRefreshIntervalMs);
    return () => clearInterval(interval);
  }, [fetchData, autoRefreshIntervalMs]);

  useEffect(() => {
    onLoadingChange?.(loading);
  }, [loading, onLoadingChange]);

  useEffect(() => {
    if (manualRefreshNonce > 0) {
      fetchData();
    }
  }, [manualRefreshNonce, fetchData]);

  useEffect(() => {
    if (!onBonusExpiring) return;
    for (const group of buildBonusGrantGroups(getAvailableResetCredits(resetCredits))) {
      const daysLeft = getDaysLeft(group.expiresAt);
      if (daysLeft != null && daysLeft <= BONUS_EXPIRY_REMINDER_DAYS) {
        onBonusExpiring(daysLeft);
      }
    }
  }, [resetCredits, onBonusExpiring]);

  const officialWeeklyLimit = selectOfficialWeeklyLimitWindow(rateLimits);
  const weeklyExhausted = isWeeklyExhausted();
  const ordinaryUsageBlocked = rateLimits?.ordinaryUsageAllowed === false;
  const availableResetCredits = getAvailableResetCredits(resetCredits);

  useEffect(() => {
    if (selectedAccountId !== 'default' && !customProfiles.some((profile) => profile.alias === selectedAccountId)) {
      setSelectedAccountId('default');
    }
  }, [customProfiles, selectedAccountId]);

  const selectedCustomProfile = selectedAccountId === 'default'
    ? null
    : customProfiles.find((profile) => profile.alias === selectedAccountId) ?? null;
  useEffect(() => {
    const limits = selectedCustomProfile
      ? {
        connected: selectedCustomProfile.status === 'connected' || selectedCustomProfile.status === 'stale',
        primary: selectedCustomProfile.primary,
        secondary: selectedCustomProfile.secondary,
        ordinaryUsageAllowed: selectedCustomProfile.ordinaryUsageAllowed,
      }
      : rateLimits;
    const available = selectedCustomProfile
      ? selectedCustomProfile.status === 'connected' || selectedCustomProfile.status === 'stale'
      : Boolean(limits?.connected && !limits.error);
    onPingContextChange?.({
      alias: selectedAccountId,
      limits,
      available,
      unavailableReason: selectedCustomProfile?.error ?? limits?.error,
    });
  }, [onPingContextChange, rateLimits, selectedAccountId, selectedCustomProfile]);
  const connected = rateLimits?.connected || codexData?.connected;
  const accountTabs = [
    { id: 'default', label: 'Default' },
    ...customProfiles.map((profile) => ({ id: profile.alias, label: profile.alias })),
  ];
  const registryMessage = registryError
    ? 'Custom profile registry unavailable.'
    : registryProvenance === 'legacy'
      ? 'Using legacy custom profile registry.'
      : registryProvenance === 'primary'
        ? 'Custom profile registry loaded.'
        : 'No custom profile registry configured.';
  const renderRegistryStatus = registryProvenance !== null ? (
    <div className={registryError ? 'error-banner' : 'codex-updated'} role={registryError ? 'alert' : 'status'}>
      {registryError && <span className="error-icon">!</span>}
      <span className={registryError ? 'error-text' : undefined}>{registryMessage}</span>
    </div>
  ) : null;
  // Cost data is always owned by the default account. Keep its component mounted
  // while a custom quota tab is selected so tab navigation cannot retrigger its
  // local IPC-backed initial load.
  const renderDefaultCostSummary = sections.cost && showCostSummary && (connected || customProfiles.length > 0) ? (
    <div
      key="codex-default-cost-summary"
      hidden={selectedCustomProfile !== null}
      aria-hidden={selectedCustomProfile !== null}
    >
      <CostSummarySection source="codex" refreshKey={manualRefreshNonce} showTrend={sections.trend} />
    </div>
  ) : null;

  if (loading && !codexData && !rateLimits) {
    return (
      <div className="codex-panel">
        <div className="loading-state">Loading Codex info...</div>
      </div>
    );
  }

  const hasRateLimits = Boolean(rateLimits?.primary || rateLimits?.secondary);
  const planType = rateLimits?.planType || codexData?.planType;
  const windows = buildCodexQuotaWindows(rateLimits);
  const topWindow = sortMostConstrained(windows)[0];
  const showingStaleLimits = Boolean(rateLimitsError && hasRateLimits);
  const quotaUnavailable = Boolean(rateLimitsError && !hasRateLimits);
  const bonusGrantGroups = buildBonusGrantGroups(availableResetCredits);
  const officialWeeklyWindow = selectOfficialWeeklyWindow(rateLimits, weeklyQuota);
  const weeklyQuotaCheck = weeklyQuota
    ? checkWeeklyQuotaWindow(weeklyQuota, officialWeeklyWindow)
    : null;
  const displayedWeeklyQuota = weeklyQuotaCheck?.ok ? weeklyQuota : null;
  const displayedWeeklyQuotaError = isHardDisplayCheck(weeklyQuotaCheck)
    ? weeklyQuotaCheck.message
    : weeklyQuotaCheck?.ok
      ? null
      : weeklyQuotaError;
  const weeklyValueCheck = officialWeeklyLimit && weeklyValueEstimate
    ? checkWeeklyValueEstimate(weeklyValueEstimate, officialWeeklyLimit)
    : null;
  const displayedWeeklyValueEstimate = officialWeeklyLimit && weeklyValueEstimate && (
    weeklyValueCheck?.ok || isSoftDisplayCheck(weeklyValueCheck)
  )
    ? weeklyValueEstimate
    : null;
  const valueIsLastEstimate = isSoftDisplayCheck(weeklyValueCheck);
  const displayedWeeklyValueEstimateError = officialWeeklyLimit && !displayedWeeklyValueEstimate
    ? (isHardDisplayCheck(weeklyValueCheck) ? null : weeklyValueEstimateError)
    : null;
  const extrasObservedAt = [
    isSoftDisplayCheck(weeklyValueCheck) && weeklyValueEstimate
      ? Date.parse(weeklyValueEstimate.observedAt)
      : Number.NaN,
    isSoftDisplayCheck(weeklyQuotaCheck) && weeklyQuota
      ? Date.parse(weeklyQuota.observedAt)
      : Number.NaN,
  ].filter((value) => Number.isFinite(value));
  const extrasPausedCopy = extrasObservedAt.length > 0
    ? formatLocalExtrasPaused(Math.min(...extrasObservedAt))
    : null;
  const renderWeeklyPace = (window: CodexRateLimitWindow) => {
    if (weeklyExhausted) return null;
    if (window !== officialWeeklyWindow) return null;
    if (isSoftDisplayCheck(weeklyQuotaCheck)) return null;
    if (!displayedWeeklyQuota && displayedWeeklyQuotaError) {
      return (
        <span className="quota-pace warning">
          Local pace unavailable: {displayedWeeklyQuotaError}
        </span>
      );
    }
    return null;
  };
  const headerStatus = showingStaleLimits
    ? 'Stale data'
    : quotaUnavailable
      ? 'Quota unavailable'
      : ordinaryUsageBlocked
        ? 'Ordinary usage blocked'
        : connected
          ? 'Connected'
          : 'Offline';
  const headerTone = showingStaleLimits
    ? 'pending'
    : quotaUnavailable || ordinaryUsageBlocked
      ? 'error'
      : connected
        ? 'online'
        : 'offline';
  const renderBonusPanel = () => {
    if (availableResetCredits.length === 0) return null;
    const body = (
      <>
        <div className="bonus-header">
          <div className="bonus-title-row">
            <span className="bonus-title">Bonus resets</span>
            <span className="bonus-badge">Gifted</span>
          </div>
          <span className="bonus-count">{availableResetCredits.length} available</span>
        </div>
        <div className="bonus-grants">
          {bonusGrantGroups.map((group) => {
            const daysLeft = getDaysLeft(group.expiresAt);
            return (
              <div className="bonus-grant-row" key={group.key}>
                <span className="bonus-grant-left">
                  <span className="bonus-dot" />
                  <span className="bonus-grant-label">
                    +{group.count} · granted {formatGrantDate(group.grantedAt)}
                  </span>
                </span>
                <span className={`bonus-grant-right ${daysLeft != null && daysLeft <= 10 ? 'warning' : ''}`}>
                  {daysLeft == null ? 'Expires unknown' : `${daysLeft}d left · ${formatGrantDate(group.expiresAt)}`}
                </span>
              </div>
            );
          })}
        </div>
        <div className="bonus-note">Gifted occasionally · no cap · each grant valid 30 days</div>
        {onOpenDashboard && (
          <div className="bonus-note">Opens ChatGPT. QuotaBar cannot apply this reset.</div>
        )}
      </>
    );
    if (!onOpenDashboard) {
      return <div className="bonus-panel">{body}</div>;
    }
    return (
      <button
        type="button"
        className="bonus-panel bonus-panel-action"
        onClick={onOpenDashboard}
      >
        {body}
      </button>
    );
  };

  const renderAccountTabs = accountTabs.length > 1 ? (
    <div className="codex-account-tabs" role="tablist" aria-label="Codex accounts">
      {accountTabs.map((account) => (
        <button
          key={account.id}
          type="button"
          role="tab"
          aria-selected={selectedAccountId === account.id}
          className={`codex-account-tab ${selectedAccountId === account.id ? 'selected' : ''}`}
          onClick={() => setSelectedAccountId(account.id)}
        >
          {account.label}
        </button>
      ))}
    </div>
  ) : null;

  if (selectedCustomProfile) {
    const status = customProfileStatus(selectedCustomProfile);
    const customLimits: CodexRateLimits = {
      connected: selectedCustomProfile.status === 'connected' || selectedCustomProfile.status === 'stale',
      planType: selectedCustomProfile.planType,
      primary: selectedCustomProfile.primary,
      secondary: selectedCustomProfile.secondary,
      ordinaryUsageAllowed: selectedCustomProfile.ordinaryUsageAllowed,
    };
    const customWindows = buildCodexQuotaWindows(customLimits);
    const customTopWindow = sortMostConstrained(customWindows)[0];
    const hasCustomLimits = Boolean(customLimits.primary || customLimits.secondary);
    const renderCustomWindow = (window: CodexRateLimitWindow, kind: 'primary' | 'secondary') => (
      <div className="quota-card" key={kind}>
        <div className="quota-header">
          <span className="quota-label">{formatWindowLabel(window.windowMinutes, kind)}</span>
          <span className="quota-value">{Math.round(window.usedPercent)}%</span>
        </div>
        <div
          className="progress-bar"
          role="progressbar"
          aria-label={`${formatWindowLabel(window.windowMinutes, kind)} usage`}
          aria-valuemin={0}
          aria-valuemax={100}
          aria-valuenow={clampProgressValue(window.usedPercent)}
          aria-valuetext={`${Math.round(window.usedPercent)}% used`}
        >
          <div className="progress-fill" style={getProgressStyle(window.usedPercent)} />
        </div>
        {window.resetsAt && (
          <div className="reset-time">
            <span>Resets in {formatResetTime(window.resetsAt)}</span>
            <span>{formatResetAt(window.resetsAt)}</span>
          </div>
        )}
      </div>
    );

    return (
      <div className="codex-panel">
        {renderRegistryStatus}
        <div className="codex-content">
          {renderAccountTabs}
          <ProviderDetailHeader
            service="codex"
            status={status.label}
            plan={formatCodexPlan(selectedCustomProfile.planType)}
            usedPercent={customTopWindow?.usedPercent ?? null}
            usageLabel={customTopWindow?.label}
            tone={status.tone}
          />
          <div className="codex-updated">{ordinaryUsageLabel(customLimits.ordinaryUsageAllowed)}</div>
          {hasCustomLimits ? (
            <div className="section">
              <div className="section-title">Usage</div>
              <div className="quota-group">
                {customLimits.primary && renderCustomWindow(customLimits.primary, 'primary')}
                {customLimits.secondary && renderCustomWindow(customLimits.secondary, 'secondary')}
                <div className="quota-card">
                  <div className="quota-header">
                    <span className="quota-label">Reset credits</span>
                    <span className="quota-value">{selectedCustomProfile.availableResetCredits}</span>
                  </div>
                </div>
              </div>
            </div>
          ) : (
            <div className="empty-state"><p>{customProfileDiagnostic(selectedCustomProfile)}</p></div>
          )}
        </div>
        {renderDefaultCostSummary}
      </div>
    );
  }

  return (
    <div className="codex-panel">
      {error && (
        <div className="error-banner">
          <span className="error-icon">!</span>
          <span className="error-text">
            {error}
            {showingStaleLimits && <span className="error-context">Showing last known data.</span>}
          </span>
        </div>
      )}

      {accountInfoError && (
        <div className="error-banner" role="alert"><span className="error-icon">!</span><span className="error-text">{accountInfoError}</span></div>
      )}

      {resetCreditsError && (
        <div className="error-banner" role="alert"><span className="error-icon">!</span><span className="error-text">{resetCreditsError}</span></div>
      )}

      {renderRegistryStatus}

      {(connected || customProfiles.length > 0) && (
        <div className="codex-content">
          {renderAccountTabs}
          <ProviderDetailHeader
            service="codex"
            status={headerStatus}
            plan={formatCodexPlan(planType)}
            usedPercent={topWindow?.usedPercent ?? null}
            usageLabel={topWindow?.label}
            tone={headerTone}
          />
          <div className="codex-updated">{ordinaryUsageLabel(rateLimits?.ordinaryUsageAllowed)}</div>
          {officialUpdatedAt != null && (
            <div className="codex-updated">
              <span>{formatOfficialUpdatedAt(officialUpdatedAt)}</span>
              {extrasPausedCopy && <span>Quota current</span>}
            </div>
          )}

          {/* Rate Limits Section */}
          {hasRateLimits && (
            <div className="section">
              <div className="section-title">Usage</div>

              <div className="quota-group">
                {rateLimits?.primary && (
                  <div className="quota-card">
                    <div className="quota-header">
                      <span className="quota-label">
                        {formatWindowLabel(rateLimits.primary.windowMinutes, 'primary')}
                      </span>
                      <span className="quota-value">
                        {Math.round(rateLimits.primary.usedPercent)}%
                      </span>
                    </div>
                    <div
                      className="progress-bar"
                      role="progressbar"
                      aria-label={`${formatWindowLabel(rateLimits.primary.windowMinutes, 'primary')} usage`}
                      aria-valuemin={0}
                      aria-valuemax={100}
                      aria-valuenow={clampProgressValue(rateLimits.primary.usedPercent)}
                      aria-valuetext={`${Math.round(rateLimits.primary.usedPercent)}% used`}
                    >
                      <div
                        className="progress-fill"
                        style={getProgressStyle(rateLimits.primary.usedPercent)}
                      />
                    </div>
                    {rateLimits.primary.resetsAt && (
                      <div className="reset-time">
                        <span>Resets in {formatResetTime(rateLimits.primary.resetsAt)}</span>
                        <span>{formatResetAt(rateLimits.primary.resetsAt)}</span>
                      </div>
                    )}
                    {!weeklyExhausted && (() => {
                      const pace = formatPaceText(
                        rateLimits.primary.usedPercent,
                        rateLimits.primary.resetsAt,
                        rateLimits.primary.windowMinutes,
                      );
                      return pace ? (
                        <span className={`quota-pace ${rateLimits.primary.usedPercent >= 50 ? 'warning' : ''}`}>
                          {pace}
                        </span>
                      ) : null;
                    })()}
                    {renderWeeklyPace(rateLimits.primary)}
                  </div>
                )}

                {rateLimits?.secondary && (
                  <div className="quota-card">
                    <div className="quota-header">
                      <span className="quota-label">
                        {formatWindowLabel(rateLimits.secondary.windowMinutes, 'secondary')}
                      </span>
                      <span className="quota-value">
                        {Math.round(rateLimits.secondary.usedPercent)}%
                      </span>
                    </div>
                    <div
                      className="progress-bar"
                      role="progressbar"
                      aria-label={`${formatWindowLabel(rateLimits.secondary.windowMinutes, 'secondary')} usage`}
                      aria-valuemin={0}
                      aria-valuemax={100}
                      aria-valuenow={clampProgressValue(rateLimits.secondary.usedPercent)}
                      aria-valuetext={`${Math.round(rateLimits.secondary.usedPercent)}% used`}
                    >
                      <div
                        className="progress-fill"
                        style={getProgressStyle(rateLimits.secondary.usedPercent)}
                      />
                    </div>
                    {rateLimits.secondary.resetsAt && (
                      <div className="reset-time">
                        <span>Resets in {formatResetTime(rateLimits.secondary.resetsAt)}</span>
                        <span>{formatResetAt(rateLimits.secondary.resetsAt)}</span>
                      </div>
                    )}
                    {renderWeeklyPace(rateLimits.secondary)}
                  </div>
                )}

                {rateLimits?.credits?.hasCredits && (
                  <div className="quota-card">
                    <div className="quota-header">
                      <span className="quota-label">Credits</span>
                      <span className="quota-value">
                        {rateLimits.credits.unlimited
                          ? 'Unlimited'
                          : formatCreditBalance(rateLimits.credits.balance)}
                      </span>
                    </div>
                  </div>
                )}
              </div>
            </div>
          )}

          {weeklyExhausted && renderBonusPanel()}

          {officialWeeklyLimit
            && (displayedWeeklyValueEstimate || displayedWeeklyValueEstimateError) && (
            <div className="section weekly-value-section">
              <div className="quota-group">
                <div className="quota-card weekly-value-card">
                  {displayedWeeklyValueEstimate ? (
                    <>
                      <div className="weekly-value-topline">
                        <span className="weekly-value-title">
                          <span className="weekly-value-dot" />
                          API-equivalent week
                        </span>
                        <span className="weekly-value-badge">
                          {valueIsLastEstimate ? 'Last estimate' : 'Local estimate'}
                        </span>
                      </div>
                      <div className="weekly-value-body">
                        <div className="weekly-value-metrics">
                          <span className="weekly-value-amount">
                            ≈{USD_FORMAT.format(displayedWeeklyValueEstimate.estimatedWeeklyValueUsd)}
                          </span>
                          <span className="weekly-value-token-row">
                            <strong>
                              ≈{COMPACT_TOKEN_FORMAT.format(displayedWeeklyValueEstimate.estimatedWeeklyTokens)}
                            </strong>
                            <span>tokens at current mix</span>
                          </span>
                        </div>
                        <div
                          className="weekly-value-gauge"
                          role="img"
                          aria-label={`Estimate based on ${Math.round(displayedWeeklyValueEstimate.usedPct)}% used`}
                          style={{
                            '--weekly-value-used': `${Math.min(Math.max(displayedWeeklyValueEstimate.usedPct, 0), 100)}%`,
                          } as CSSProperties}
                        >
                          <span className="weekly-value-gauge-center">
                            <strong>{Math.round(displayedWeeklyValueEstimate.usedPct)}%</strong>
                            <small>used</small>
                          </span>
                        </div>
                      </div>
                      <div className="weekly-value-footer weekly-value-footer-basis">
                        <span>
                          {`Based on ${Math.round(displayedWeeklyValueEstimate.usedPct)}% used · ${USD_FORMAT.format(displayedWeeklyValueEstimate.observedCostUsd)} local`}
                        </span>
                        <span>
                          {valueIsLastEstimate
                            ? `${COMPACT_TOKEN_FORMAT.format(displayedWeeklyValueEstimate.observedTokens)} observed tokens · Not an official allowance · snapshot not refreshed`
                            : `${COMPACT_TOKEN_FORMAT.format(displayedWeeklyValueEstimate.observedTokens)} observed tokens · Not an official allowance`}
                        </span>
                        <span>
                          Standard API prices · Not a bill
                        </span>
                      </div>
                    </>
                  ) : (
                    <span className="quota-pace warning">
                      Weekly value unavailable: {displayedWeeklyValueEstimateError}
                    </span>
                  )}
                </div>
              </div>
            </div>
          )}

          {extrasPausedCopy && (
            <p className="codex-local-extras">{extrasPausedCopy}</p>
          )}

          {sections.tips && (
            <SmartTip message={getHighUsageTip(windows)} />
          )}

          {!weeklyExhausted && renderBonusPanel()}

          {sections.timeline && <ResetTimeline windows={windows} />}

          {/* Subscription Section (only if no rate limits) */}
          {!hasRateLimits && codexData && (
            <div className="section">
              <div className="section-title">Subscription</div>
              <div className="quota-group">
                <div className="quota-card">
                  <div className="quota-header">
                    <span className="quota-label">Plan</span>
                    <span className="quota-value plan-badge">
                      {formatPlanType(planType)}
                    </span>
                  </div>
                </div>
                <div className="quota-card">
                  <div className="quota-header">
                    <span className="quota-label">Valid Until</span>
                    <span className="quota-value">
                      {formatSubscriptionDate(codexData.subscriptionUntil)}
                    </span>
                  </div>
                </div>
                {codexData.email && (
                  <div className="quota-card">
                    <div className="quota-header">
                      <span className="quota-label">Account</span>
                      <span className="quota-value email">{codexData.email}</span>
                    </div>
                  </div>
                )}
              </div>
            </div>
          )}

        </div>
      )}

      {renderDefaultCostSummary}

      {!connected && !error && customProfiles.length === 0 && (
        <div className="empty-state">
          <p>Codex not connected</p>
          <p className="hint">Run 'codex' in terminal to login</p>
        </div>
      )}
    </div>
  );
}
