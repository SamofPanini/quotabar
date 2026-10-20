import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { backend, hasTauriBackend } from './backend';
import { readStorageValue, writeStorageItem } from './storage';

export type ServiceStatusLevel = 'operational' | 'maintenance' | 'degraded' | 'partial_outage' | 'major_outage' | 'unknown';
export type ServiceAlert = 'none' | 'warning' | 'critical';
export interface ServiceStatusUpdate { id: string; status: string; body: string; updatedAt?: string }
export interface ServiceStatusEvent { id: string; name: string; status: string; impact?: string; url?: string; latestUpdate?: ServiceStatusUpdate; startedAt?: string; affectedComponents?: string[]; alert?: ServiceAlert }
export interface ResolvedServiceEvent extends ServiceStatusEvent { resolvedAt: string; resolvedAtEstimated: boolean }
export interface ProviderServiceStatus { provider: 'claude' | 'codex'; level: ServiceStatusLevel; components: Array<{ name: string; status: string; tier?: string }>; incidents: ServiceStatusEvent[]; maintenances: ServiceStatusEvent[]; recentlyResolved?: ResolvedServiceEvent[]; fetchedAt?: string; error?: string; alert?: ServiceAlert }
export interface ServiceStatusSnapshot { claude: ProviderServiceStatus; codex: ProviderServiceStatus }

const ENABLED_KEY = 'quotabar-service-status-enabled';
export function getServiceStatusEnabled(): boolean {
  const result = readStorageValue(ENABLED_KEY, (raw) => {
    if (raw === 'true') return true;
    if (raw === 'false') return false;
    throw new Error('Invalid service status setting');
  }, { notifyUser: true });
  return result.status === 'value' ? result.value : true;
}
export function setServiceStatusEnabled(enabled: boolean): boolean {
  return writeStorageItem(ENABLED_KEY, String(enabled), { preserveSessionValue: true, notifyUser: true });
}
export function getServiceStatus(): Promise<ServiceStatusSnapshot> { return backend.getServiceStatus(); }
export function onServiceStatusChanged(handler: (status: ServiceStatusSnapshot) => void): Promise<UnlistenFn> {
  if (!hasTauriBackend()) return Promise.resolve(() => {});
  return listen<ServiceStatusSnapshot>('service-status-changed', (event) => handler(event.payload));
}
export function isServiceIncident(status: ProviderServiceStatus | undefined): boolean {
  return status != null && (['degraded', 'partial_outage', 'major_outage'].includes(status.level) || status.incidents.length > 0);
}
export function claudePanelErrorWithServiceIncident(
  error: string | null,
  authError: boolean,
  loginRefreshMessage: string | null,
  status: ProviderServiceStatus | undefined,
): string | null {
  if (!error) return null;
  const primary = authError ? (loginRefreshMessage ?? error) : error;
  return `${primary}${isServiceIncident(status) ? ' Anthropic reports an incident.' : ''}`;
}
export function serviceStatusForDisplay(
  enabled: boolean,
  snapshot: ServiceStatusSnapshot,
): ServiceStatusSnapshot | null {
  return enabled ? snapshot : null;
}
export function serviceStatusLabel(status: ProviderServiceStatus): string {
  const labels: Partial<Record<ServiceStatusLevel, string>> = {
    degraded: 'Degraded', partial_outage: 'Partial outage', major_outage: 'Major outage', maintenance: 'Maintenance',
  };
  return labels[status.level] ?? '';
}
export function formatServiceStatusDate(value: string | undefined, now = new Date(), timeZone?: string): string {
  if (!value || Number.isNaN(new Date(value).getTime())) return '';
  const date = new Date(value);
  const currentYear = new Intl.DateTimeFormat('en-US', { year: 'numeric', timeZone }).format(now);
  const year = new Intl.DateTimeFormat('en-US', { year: 'numeric', timeZone }).format(date);
  const options: Intl.DateTimeFormatOptions = { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit', hourCycle: 'h23', timeZone };
  if (year !== currentYear) options.year = 'numeric';
  return new Intl.DateTimeFormat('en-US', options).format(date).replace(',', ',');
}
export function componentStatusLabel(status: string): string {
  return ({ degraded_performance: 'Degraded performance', partial_outage: 'Partial outage', major_outage: 'Major outage', full_outage: 'Major outage' } as Record<string, string>)[status] ?? status;
}
export function serviceStageLabel(event: ServiceStatusEvent, isMaintenance = false): string {
  if (isMaintenance) return 'Maintenance';
  return event.status ? event.status[0].toUpperCase() + event.status.slice(1) : 'Incident';
}
function serviceEventRank(event: ServiceStatusEvent): number {
  return event.alert === 'critical' ? 2 : event.alert === 'warning' ? 1 : 0;
}
function eventUpdateInstant(event: ServiceStatusEvent): number | null {
  const value = event.latestUpdate?.updatedAt;
  if (!value) return null;
  const instant = Date.parse(value);
  return Number.isNaN(instant) ? null : instant;
}
export function orderedServiceEvents(status: ProviderServiceStatus): ServiceStatusEvent[] {
  return [...status.incidents, ...status.maintenances].sort((a, b) => {
    const rank = serviceEventRank(b) - serviceEventRank(a);
    if (rank) return rank;
    const left = eventUpdateInstant(a);
    const right = eventUpdateInstant(b);
    if (left != null && right != null) return right - left;
    if (left != null) return -1;
    if (right != null) return 1;
    return 0;
  });
}
export function serviceTimesMatch(startedAt: string | undefined, updatedAt: string | undefined): boolean {
  if (!startedAt || !updatedAt) return false;
  const started = Date.parse(startedAt);
  const updated = Date.parse(updatedAt);
  return !Number.isNaN(started) && !Number.isNaN(updated) && started === updated;
}
