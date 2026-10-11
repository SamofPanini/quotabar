import CostSummarySection from './CostSummarySection';
import QuotaCard from './QuotaCard';
import ProviderDetailHeader from './ProviderDetailHeader';
import ResetTimeline from './ResetTimeline';
import SmartTip from './SmartTip';
import type { QuotaData } from '../types/models';
import { formatPaceText, formatResetTime } from '../utils/quota_format';
import { buildClaudeQuotaWindows, sortMostConstrained } from '../services/provider_summary';
import { getHighUsageTip } from '../services/detail_helpers';
import { defaultPanelSections, type PanelSectionVisibility } from '../services/panel_sections';
import ServiceStatusNotice from './ServiceStatusNotice';
import type { ProviderServiceStatus } from '../services/service_status';
import { formatServiceStatusDate } from '../services/service_status';

interface ClaudePanelProps {
  quota: QuotaData | null;
  loading: boolean;
  error: string | null;
  windowVisible: boolean;
  costRefreshKey: number;
  onRetry: () => void;
  sections?: PanelSectionVisibility;
  serviceStatus?: ProviderServiceStatus;
}

function formatClaudeResetTime(resetTime?: string): string {
  return formatResetTime(resetTime, {
    emptyLabel: 'N/A',
    expiredLabel: 'Soon',
    showZeroHours: true,
  });
}

function hasWeeklyData(quota: QuotaData): boolean {
  return Boolean(
    quota.weeklyTotal ||
      quota.weeklyOpus ||
      quota.weeklySonnet ||
      quota.weeklyDesign ||
      quota.weeklyFable5,
  );
}

function staleClaudeNotice(quota: QuotaData): string | null {
  if (!quota.staleReason || !quota.staleSince) return null;
  const at = formatServiceStatusDate(quota.staleSince);
  if (!at) return null;
  if (quota.staleReason === 'signed_out') {
    return `Signed out · showing values from ${at} · sign in with claude in Terminal`;
  }
  return quota.autoRenew === 'pending' || quota.autoRenew === 'sent'
    ? `Login expired · showing values from ${at} · renewing automatically`
    : 'Login expired · showing values from ' + at + ' · open Claude Code or press Ping to renew';
}

export default function ClaudePanel({
  quota,
  loading,
  error,
  windowVisible,
  costRefreshKey,
  onRetry,
  sections = defaultPanelSections(),
  serviceStatus,
}: ClaudePanelProps) {
  const windows = buildClaudeQuotaWindows(quota);
  const topWindow = sortMostConstrained(windows)[0];
  const staleNotice = quota ? staleClaudeNotice(quota) : null;
  const grayed = quota?.staleReason != null;

  return (
    <>
      {loading && !quota && (
        <div className="loading-state">Loading Claude quota...</div>
      )}

      {error && !staleNotice && (
        <div className="error-banner" role="alert">
          <span className="error-icon">!</span>
          <span className="error-text">
            {error}
            {quota && <span className="error-context">Showing last known data.</span>}
          </span>
        </div>
      )}

      {!quota && <ServiceStatusNotice provider="claude" status={serviceStatus} />}

      {quota && (
        <div className="detail-stack">
          <ProviderDetailHeader
            service="claude"
            status={error ? 'Stale data' : quota.connected ? 'Connected' : 'Offline'}
            plan="Claude Code"
            usedPercent={topWindow?.usedPercent ?? null}
            usageLabel={topWindow?.label}
            tone={error ? 'pending' : quota.connected ? 'online' : 'offline'}
          />
          <ServiceStatusNotice provider="claude" status={serviceStatus} />
          {staleNotice && <div className="claude-stale-notice">{staleNotice}</div>}

          <div className="section">
            <div className="section-title">Current session</div>
            <div className="quota-group">
              {quota.session ? (
                <QuotaCard
                  label="5-hour window"
                  percentage={Math.round(quota.session.percentage)}
                  resetsIn={formatClaudeResetTime(quota.session.resetTime)}
                  resetAt={quota.session.resetTime}
                  pace={formatPaceText(quota.session.percentage, quota.session.resetTime, 5 * 60)}
                  grayed={grayed}
                />
              ) : (
                <div className="no-data">No session data</div>
              )}
            </div>
          </div>

          <div className="section">
            <div className="section-title">Weekly limits</div>
            <div className="quota-group">
              {quota.weeklyTotal && (
                <QuotaCard
                  label="All models"
                  percentage={Math.round(quota.weeklyTotal.percentage)}
                  resetsIn={formatClaudeResetTime(quota.weeklyTotal.resetTime)}
                  resetAt={quota.weeklyTotal.resetTime}
                  featured
                  grayed={grayed}
                />
              )}

              {quota.weeklyOpus && (
                <QuotaCard
                  label="Opus"
                  percentage={Math.round(quota.weeklyOpus.percentage)}
                  resetsIn={formatClaudeResetTime(quota.weeklyOpus.resetTime)}
                  resetAt={quota.weeklyOpus.resetTime}
                  grayed={grayed}
                />
              )}

              {quota.weeklySonnet && (
                <QuotaCard
                  label="Sonnet"
                  percentage={Math.round(quota.weeklySonnet.percentage)}
                  resetsIn={formatClaudeResetTime(quota.weeklySonnet.resetTime)}
                  resetAt={quota.weeklySonnet.resetTime}
                  grayed={grayed}
                />
              )}

              {quota.weeklyDesign && (
                <QuotaCard
                  label="Claude Design"
                  percentage={Math.round(quota.weeklyDesign.percentage)}
                  resetsIn={formatClaudeResetTime(quota.weeklyDesign.resetTime)}
                  resetAt={quota.weeklyDesign.resetTime}
                  grayed={grayed}
                />
              )}

              {quota.weeklyFable5 && (
                <QuotaCard
                  label="Fable 5"
                  percentage={Math.round(quota.weeklyFable5.percentage)}
                  resetsIn={formatClaudeResetTime(quota.weeklyFable5.resetTime)}
                  resetAt={quota.weeklyFable5.resetTime}
                  grayed={grayed}
                />
              )}

              {!hasWeeklyData(quota) && (
                <div className="no-data">No weekly data</div>
              )}
            </div>
          </div>

          {sections.tips && <SmartTip message={getHighUsageTip(windows)} />}

          {sections.timeline && <ResetTimeline windows={windows} />}

          {sections.cost && windowVisible && (
            <CostSummarySection source="claude" refreshKey={costRefreshKey} showTrend={sections.trend} />
          )}
        </div>
      )}

      {!quota && !loading && (
        <div className="empty-state">
          <p>Unable to load quota data</p>
          <button type="button" onClick={onRetry} className="retry-btn">
            Try Again
          </button>
        </div>
      )}
    </>
  );
}
