import { componentStatusLabel, formatServiceStatusDate, orderedServiceEvents, serviceStageLabel, serviceTimesMatch, type ProviderServiceStatus } from '../services/service_status';
import { backend } from '../services/backend';

const displayName = (name: string) => name.replace(/\s*\([^)]*\).*/, '');

export default function ServiceStatusNotice({ provider, status, timeZone }: { provider: 'claude' | 'codex'; status?: ProviderServiceStatus; timeZone?: string }) {
  const resolvedCutoff = Date.now() - 24 * 60 * 60 * 1000;
  const recentlyResolved = (status?.recentlyResolved ?? []).filter((event) => {
    const resolvedAt = Date.parse(event.resolvedAt);
    return !Number.isNaN(resolvedAt) && resolvedAt >= resolvedCutoff;
  });
  if (!status || (status.level === 'operational' && !status.incidents.length && !status.maintenances.length && !recentlyResolved.length)) return null;
  if (status.level === 'unknown') return <div className="service-status-notice unavailable">Status unavailable</div>;
  const providerName = provider === 'claude' ? 'Anthropic' : 'OpenAI';
  const events = orderedServiceEvents(status);
  const shown = events.slice(0, 2);
  const componentByName = new Map(status.components.map((component) => [displayName(component.name), component]));
  const affected = [...new Set([
    ...events.flatMap((event) => event.affectedComponents ?? []),
    ...status.components.filter((component) => component.status !== 'operational').map((component) => component.name),
  ].map(displayName))]
    .sort((a, b) => Number(componentByName.get(b)?.tier === 'core') - Number(componentByName.get(a)?.tier === 'core'))
    .join(', ');
  const fallback = status.components.find((component) => component.status !== 'operational');
  const tone = status.alert ?? 'none';
  const resolvedOnly = !events.length && !fallback;
  const affectsConsole = provider === 'claude' && (status.components.some((component) => component.name.startsWith('Claude Console') && component.status !== 'operational') || status.incidents.some((event) => event.affectedComponents?.some((name) => name.startsWith('Claude Console'))));
  return <div className={`service-status-notice ${resolvedOnly ? 'resolved' : tone === 'none' ? 'maintenance' : tone}`} role="status">
    <div className="service-status-provider"><span className={`service-status-provider-dot ${resolvedOnly ? 'none' : tone}`} aria-hidden="true">● </span><strong>{providerName}{affected ? ` · ${affected}` : ''}</strong></div>
    {shown.map((event) => <div className="service-status-event" key={event.id}>
      <div><span className={`service-status-stage ${event.alert ?? 'none'}`}>[{serviceStageLabel(event, status.maintenances.includes(event))}]</span> {event.name}{event.url && <button type="button" className="service-status-open-link" onClick={() => { void backend.openServiceStatusUrl(event.url!); }} aria-label="Open status incident">↗</button>}</div>
      {event.startedAt && <div className="service-status-time">Started {formatServiceStatusDate(event.startedAt, new Date(), timeZone)}{event.latestUpdate?.updatedAt && !serviceTimesMatch(event.startedAt, event.latestUpdate.updatedAt) ? ` · Updated ${formatServiceStatusDate(event.latestUpdate.updatedAt, new Date(), timeZone)}` : ''}</div>}
    </div>)}
    {!shown.length && fallback && <div className="service-status-event"><span className={`service-status-stage ${tone}`}>[{componentStatusLabel(fallback.status)}]</span> {displayName(fallback.name)}</div>}
    {recentlyResolved.slice(0, 2).map((event) => <div className="service-status-event service-status-resolved" key={`resolved-${event.id}`}>
      <div><span className="service-status-stage none">[Resolved]</span> {event.name}{event.url && <button type="button" className="service-status-open-link" onClick={() => { void backend.openServiceStatusUrl(event.url!); }} aria-label="Open status incident">↗</button>}</div>
      <div className="service-status-time">{event.startedAt && `Started ${formatServiceStatusDate(event.startedAt, new Date(), timeZone)} · `}{event.resolvedAtEstimated ? 'Resolved by ' : 'Resolved '}{formatServiceStatusDate(event.resolvedAt, new Date(), timeZone)}</div>
    </div>)}
    {(Math.max(0, events.length - 2) + Math.max(0, recentlyResolved.length - 2)) > 0 && <button type="button" className="service-status-more" onClick={() => { const url = events[0]?.url ?? recentlyResolved[0]?.url; if (url) void backend.openServiceStatusUrl(url); }}>+{Math.max(0, events.length - 2) + Math.max(0, recentlyResolved.length - 2)} more on the status page</button>}
    {affectsConsole && <div>Affects login renewal</div>}
  </div>;
}
