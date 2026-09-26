export interface BonusReadySnapshot {
  exhausted: boolean;
  availableCount: number;
}

export function canReportBonusReady(
  _resetCredits: { connected: boolean; availableCount?: number } | null,
  _officialWeeklyUsedPercent?: number,
  _filteredAvailableCount?: number,
): boolean {
  // No authoritative exhaustion reason is currently modeled. A percentage,
  // ordinary-use permission, or reset credit cannot establish bonus readiness.
  return false;
}

export function bonusReadyEntered(
  _prev: BonusReadySnapshot | null,
  _next: BonusReadySnapshot,
): boolean {
  return false;
}

export function formatBonusReadyMessage(availableCount: number): string {
  const noun = availableCount === 1 ? 'bonus reset' : 'bonus resets';
  return `${availableCount} Codex ${noun} available.`;
}
