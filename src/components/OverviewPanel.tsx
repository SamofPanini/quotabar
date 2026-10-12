import type { ProviderSummary, QuotaWindowSummary } from '../services/provider_summary';
import type { TrayServiceName } from '../services/tray_visibility';
import { clampProgressValue, getProgressStyle } from '../utils/quota_format';
import CostSummarySection, { ALL_COST_DISPLAY_NAME } from './CostSummarySection';
import ProviderDetailHeader from './ProviderDetailHeader';
import ResetTimeline from './ResetTimeline';
import { defaultPanelSections, type PanelSectionVisibility } from '../services/panel_sections';
import { componentStatusLabel, orderedServiceEvents, serviceStageLabel, type ServiceStatusSnapshot } from '../services/service_status';

const ALL_COST_SOURCES = ['claude', 'codex', 'cursor'] as const;

interface OverviewPanelProps {
  summaries: ProviderSummary[];
  mostConstrained: QuotaWindowSummary[];
  upcomingResets: QuotaWindowSummary[];
  costRefreshKey: number;
  showCostSummary?: boolean;
  onProviderSelect: (provider: TrayServiceName) => void;
  sections?: PanelSectionVisibility;
  serviceStatus?: ServiceStatusSnapshot | null;
  claudeStaleReason?: 'expired' | 'signed_out';
}

export default function OverviewPanel({
  summaries,
  mostConstrained,
  upcomingResets,
  costRefreshKey,
  showCostSummary = true,
  onProviderSelect,
  sections = defaultPanelSections(),
  serviceStatus,
  claudeStaleReason,
}: OverviewPanelProps) {
  const connectedCount = summaries.filter((summary) => summary.connected).length;

  return (
    <div className="overview-panel">
      <ProviderDetailHeader
        service="claude"
        label="Overview"
        status={`${connectedCount} of ${summaries.length} connected`}
        plan={ALL_COST_DISPLAY_NAME}
        usedPercent={mostConstrained[0]?.usedPercent ?? null}
        usageLabel={mostConstrained[0] ? `${mostConstrained[0].providerLabel} · ${mostConstrained[0].label}` : undefined}
        tone={connectedCount > 0 ? 'online' : 'offline'}
      />

      {serviceStatus && (['claude', 'codex'] as const).some((provider) => serviceStatus[provider].level !== 'unknown') && (
        <div className="service-status-overview" aria-label="Service status">
          {(['claude', 'codex'] as const).map((provider) => {
            const status = serviceStatus[provider];
            if (status.level === 'unknown') return null;
            const name = provider === 'claude' ? 'Anthropic' : 'OpenAI';
            const event = orderedServiceEvents(status)[0];
            const component = status.components.find((item) => item.status !== 'operational');
            const label = event ? serviceStageLabel(event, status.maintenances.includes(event)) : component ? componentStatusLabel(component.status) : '';
            const detail = event ? `${event.name} · ${event.affectedComponents?.join(', ') ?? ''}` : component?.name ?? '';
            return <span key={provider} className="service-status-overview-item">{name}{status.level === 'operational' ? <span className="service-status-dot" title={`${name}: all watched services operational`} /> : <span className={`service-status-pill ${status.alert ?? 'none'}`} title={`${name}: ${detail}`}>{label}</span>}</span>;
          })}
        </div>
      )}

      <div className="section">
        <div className="section-title">Most constrained</div>
        <div className="quota-group">
          {mostConstrained.length > 0 ? mostConstrained.map((window, index) => {
            return (
            <button
              type="button"
              className={`quota-card overview-quota-row${index === 0 ? ' primary' : ''}${window.provider === 'claude' && claudeStaleReason ? ' quota-card-stale' : ''}`}
              key={`${window.provider}-${window.label}`}
              onClick={() => onProviderSelect(window.provider)}
              aria-label={`Open ${window.providerLabel}: ${window.label}, ${Math.round(window.usedPercent)}% used`}
            >
              <div className="quota-header">
                <span className="quota-label">{`${window.providerLabel} · ${window.label}`}</span>
                <span className="quota-value">{Math.round(window.usedPercent)}%</span>
              </div>
              <div
                className="progress-bar"
                role="progressbar"
                aria-label={`${window.providerLabel} ${window.label} usage`}
                aria-valuemin={0}
                aria-valuemax={100}
                aria-valuenow={clampProgressValue(window.usedPercent)}
                aria-valuetext={`${Math.round(window.usedPercent)}% used`}
              >
                <div className={`progress-fill${window.provider === 'claude' && claudeStaleReason ? ' stale' : ''}`} style={getProgressStyle(window.usedPercent)} />
              </div>
              {window.resetLabel && <div className="reset-time">Resets in {window.resetLabel}</div>}
              {window.provider === 'claude' && claudeStaleReason && <small className="overview-stale-label">{claudeStaleReason === 'expired' ? 'expired' : 'signed out'}</small>}
            </button>
          ); }) : (
            <div className="no-data">No provider data</div>
          )}
        </div>
      </div>

      {sections.timeline && <ResetTimeline windows={upcomingResets} />}
      {sections.cost && showCostSummary && (
        <CostSummarySection source={ALL_COST_SOURCES} refreshKey={costRefreshKey} showTrend={sections.trend} />
      )}
    </div>
  );
}
