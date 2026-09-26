import { describe, expect, test } from 'vitest';
import { bonusReadyEntered, canReportBonusReady, formatBonusReadyMessage } from '../src/services/bonus_ready';

describe('canReportBonusReady', () => {
  test('does not infer bonus readiness from credits or a numeric weekly window', () => {
    expect(canReportBonusReady(null, 100)).toBe(false);
    expect(canReportBonusReady({ connected: false }, 100)).toBe(false);
    expect(canReportBonusReady({ connected: true })).toBe(false);
    expect(canReportBonusReady({ connected: true }, Number.NaN)).toBe(false);
    expect(canReportBonusReady({ connected: true }, 40)).toBe(false);
    expect(canReportBonusReady({ connected: true }, 100)).toBe(false);
  });

  test('does not infer bonus readiness when counts agree', () => {
    expect(canReportBonusReady({ connected: true, availableCount: 1 }, 100, 0)).toBe(false);
    expect(canReportBonusReady({ connected: true, availableCount: 1 }, 100, 1)).toBe(false);
    expect(canReportBonusReady({ connected: true, availableCount: 0 }, 100, 0)).toBe(false);
  });
});

describe('bonusReadyEntered', () => {
  test('does not fire on the first snapshot', () => {
    expect(bonusReadyEntered(null, { exhausted: true, availableCount: 1 })).toBe(false);
  });

  test('does not fire from an unqualified exhausted transition', () => {
    expect(bonusReadyEntered(
      { exhausted: false, availableCount: 1 },
      { exhausted: true, availableCount: 1 },
    )).toBe(false);
  });

  test('does not fire when a credit arrives with an unqualified exhausted state', () => {
    expect(bonusReadyEntered(
      { exhausted: true, availableCount: 0 },
      { exhausted: true, availableCount: 1 },
    )).toBe(false);
  });

  test('does not fire when exhausted with an unchanged credit', () => {
    expect(bonusReadyEntered(
      { exhausted: true, availableCount: 1 },
      { exhausted: true, availableCount: 1 },
    )).toBe(false);
  });

  test('does not fire at 100% with zero credits', () => {
    expect(bonusReadyEntered(
      { exhausted: false, availableCount: 0 },
      { exhausted: true, availableCount: 0 },
    )).toBe(false);
  });
});

describe('formatBonusReadyMessage', () => {
  test('uses singular copy for one credit', () => {
    expect(formatBonusReadyMessage(1)).toBe('1 Codex bonus reset available.');
  });

  test('uses plural copy for multiple credits', () => {
    expect(formatBonusReadyMessage(2)).toBe('2 Codex bonus resets available.');
  });
});
