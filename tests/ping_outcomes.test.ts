import { describe, expect, it } from 'vitest';
import fixtures from './fixtures/ping_outcomes.json';
import { formatPingReset } from '../src/services/ping_window';
import { PING_OUTCOME_KINDS, type PingOutcome } from '../src/types/models';

describe('PING1 PingOutcome cross-language fixture', () => {
  it('covers every models.ts kind and the fields the UI reads', () => {
    const outcomes = fixtures as PingOutcome[];
    expect(outcomes.map((outcome) => outcome.kind).sort()).toEqual(Object.keys(PING_OUTCOME_KINDS).sort());

    const opened = outcomes.find((outcome) => outcome.kind === 'opened');
    expect(opened).toBeDefined();
    if (opened?.kind === 'opened') {
      expect(typeof opened.resetsAt).toBe('number');
      expect(typeof opened.confirmedAfterSecs).toBe('number');
      expect(formatPingReset(opened.resetsAt)).not.toBe('unknown time');
    }

    const unconfirmed = outcomes.find((outcome) => outcome.kind === 'sentUnconfirmed');
    expect(unconfirmed).toBeDefined();
    if (unconfirmed?.kind === 'sentUnconfirmed') {
      expect(typeof unconfirmed.expectedResetsAt).toBe('number');
    }

    const confirming = outcomes.find((outcome) => outcome.kind === 'confirming');
    expect(confirming).toBeDefined();
    if (confirming?.kind === 'confirming') {
      expect(typeof confirming.expectedResetsAt).toBe('number');
    }

    const alreadyOpen = outcomes.find((outcome) => outcome.kind === 'alreadyOpen');
    expect(alreadyOpen).toBeDefined();
    if (alreadyOpen?.kind === 'alreadyOpen') {
      expect(alreadyOpen.resetsAt).toBeNull();
    }
  });
});
