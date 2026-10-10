import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { backend, hasTauriBackend } from './backend';
import { readStorageValue, writeStorageItem } from './storage';

export type ServiceStatusLevel = 'operational' | 'maintenance' | 'degraded' | 'partial_outage' | 'major_outage' | 'unknown';
export interface ServiceStatusUpdate { id: string; status: string; body: string; updatedAt?: string }
export interface ServiceStatusEvent { id: string; name: string; status: string; impact?: string; url?: string; latestUpdate?: ServiceStatusUpdate }
export interface ProviderServiceStatus { provider: 'claude' | 'codex'; level: ServiceStatusLevel; components: Array<{ name: string; status: string }>; incidents: ServiceStatusEvent[]; maintenances: ServiceStatusEvent[]; fetchedAt?: string; error?: string }
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
