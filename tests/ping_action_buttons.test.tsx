import { describe, expect, it } from 'vitest';
import { renderToStaticMarkup } from 'react-dom/server';
import type { ComponentProps } from 'react';
import ActionButtons from '../src/components/ActionButtons';

function render(overrides: Partial<ComponentProps<typeof ActionButtons>> = {}) {
  return renderToStaticMarkup(<ActionButtons
    onRefresh={() => {}}
    onDashboard={() => {}}
    onSettings={() => {}}
    onQuit={() => {}}
    loading={false}
    {...overrides}
  />);
}

describe('Ping footer action', () => {
  it('is absent unless a supported provider opts in and stays after Refresh as P', () => {
    expect(render()).not.toContain('Ping');
    const html = render({ showPing: true });
    expect(html.indexOf('Refresh')).toBeLessThan(html.indexOf('>P<'));
    expect(html.indexOf('>P<')).toBeLessThan(html.indexOf('Dashboard'));
    expect(html).not.toContain('>Ping<');
    expect(html).toContain('aria-label="Ping selected account — start 5-hour window"');
  });

  it('uses the safe disabled title and disables in flight', () => {
    const disabled = render({ showPing: true, pingDisabledReason: 'Ordinary usage blocked — ping would not open a window' });
    expect(disabled).toContain('title="Ordinary usage blocked — ping would not open a window"');
    expect(disabled).toContain('disabled=""');
    const inFlight = render({ showPing: true, pingState: 'inFlight' });
    expect(inFlight).toContain('>...<');
    expect(inFlight).not.toContain('Pinging…');
    expect(inFlight).toContain('disabled=""');
    expect(inFlight).toContain('title="Ping is in progress"');
    const refreshLoading = render({ showPing: true, loading: true });
    expect(refreshLoading).toContain('title="Refresh is in progress"');
  });

  it('puts the confirmation in its own one-line strip above the footer action row', () => {
    const html = render({ showPing: true, pingConfirmText: 'A long confirmation that must not hide either action' });
    expect(html.indexOf('ping-confirm')).toBeLessThan(html.indexOf('footer-divider'));
    expect(html).toContain('class="ping-confirm-text"');
    expect(html).toContain('>Send<');
    expect(html).toContain('>Cancel<');
  });
});
