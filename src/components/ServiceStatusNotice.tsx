import type { ProviderServiceStatus } from '../services/service_status';
import { backend } from '../services/backend';

export default function ServiceStatusNotice({ provider, status }: { provider: 'claude' | 'codex'; status?: ProviderServiceStatus }) {
  if (!status || status.level === 'operational') return null;
  if (status.level === 'unknown') return <div className="service-status-notice unavailable">Status unavailable</div>;
  const providerName = provider === 'claude' ? 'Anthropic' : 'OpenAI';
  const label = ({ degraded: 'degraded', partial_outage: 'partial outage', major_outage: 'major outage', maintenance: 'maintenance' } as const)[status.level] ?? 'unavailable';
  const event = status.incidents[0] ?? status.maintenances[0];
  const affectsConsole = provider === 'claude' && status.components.some((component) => component.name.startsWith('Claude Console') && component.status !== 'operational');
  const updated = event?.latestUpdate?.updatedAt ? new Date(event.latestUpdate.updatedAt).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', hour12: false }) : null;
  return (
    <div className={`service-status-notice ${status.level}`} role="status">
      <div><span aria-hidden="true">● </span><strong>{providerName}: {label}</strong>{event?.url && <button type="button" onClick={() => { void backend.openServiceStatusUrl(event.url!); }} aria-label="Open status incident"> ↗</button>}</div>
      {event && <div>{event.name} · {event.latestUpdate?.status ?? event.status}{updated ? ` · ${updated}` : ''}</div>}
      {affectsConsole && <div>Affects login renewal</div>}
    </div>
  );
}
