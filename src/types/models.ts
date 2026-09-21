export interface UsageInfo {
  used: number;
  limit: number;
  percentage: number;
  resetTime?: string;
}

export interface QuotaData {
  connected: boolean;
  session?: UsageInfo;
  weeklyTotal?: UsageInfo;
  weeklyOpus?: UsageInfo;
  weeklySonnet?: UsageInfo;
  weeklyDesign?: UsageInfo;
  weeklyFable5?: UsageInfo;
  error?: string;
}

/** Read-only C3-A projection. It intentionally omits binding, sequence, source, and path data. */
export type ClaudeBindingState = 'unbound' | 'bound' | 'unverified' | 'error';
export type ClaudeWindowStatus = 'fresh' | 'stale' | 'expired' | 'unavailable';
export type ClaudeSafeErrorCode = 'unavailable' | 'malformed_payload' | 'unsupported_observation' | 'clock_rollback';

export interface ClaudeWindowProjection {
  status: ClaudeWindowStatus;
  usedPercent?: number;
  resetAt?: string;
  observedAt?: string;
  lastErrorCode?: ClaudeSafeErrorCode;
}

export interface ClaudeCurrentSlot {
  alias: string;
  bindingState: ClaudeBindingState;
  plan?: 'paid' | 'free';
  fiveHour: ClaudeWindowProjection;
  weekly: ClaudeWindowProjection;
}

export interface ClaudeCurrentSnapshots {
  slots: ClaudeCurrentSlot[];
}

export interface CodexData {
  connected: boolean;
  planType?: string;
  accountId?: string;
  subscriptionUntil?: string;
  email?: string;
  error?: string;
}

export interface CodexRateLimitWindow {
  usedPercent: number;
  windowMinutes?: number;
  resetsAt?: number;
}

export interface CodexCredits {
  hasCredits: boolean;
  unlimited: boolean;
  balance?: string;
}

export interface CodexRateLimits {
  connected: boolean;
  planType?: string;
  primary?: CodexRateLimitWindow;
  secondary?: CodexRateLimitWindow;
  credits?: CodexCredits;
  error?: string;
}

export interface CodexResetCredit {
  status: string;
  title?: string;
  grantedAt?: string;
  expiresAt?: string;
}

export interface CodexResetCredits {
  connected: boolean;
  availableCount: number;
  credits: CodexResetCredit[];
  error?: string;
}

/** The intentionally path-free public DTO returned by get_codex_profiles. */
export interface CodexProfileQuota {
  alias: string;
  status: 'connected' | 'stale' | 'offline' | 'error';
  planType?: string;
  primary?: CodexRateLimitWindow;
  secondary?: CodexRateLimitWindow;
  availableResetCredits: number;
  error?: string;
}

export interface CodexProfilesResponse {
  profiles: CodexProfileQuota[];
  registryError?: string | null;
}

export type CodexQuotaStatus = 'on_track' | 'watch' | 'likely_exhausted' | 'exhausted';

export interface CodexWeeklyQuota {
  observedAt: string;
  resetsAt: string;
  estimatedDepletionAt?: string;
  windowMinutes: number;
  usedPct: number;
  remainingPct: number;
  projectedPctAtReset: number;
  status: CodexQuotaStatus;
}

export interface CodexWeeklyValueEstimate {
  observedAt: string;
  windowStartedAt: string;
  resetsAt: string;
  usedPct: number;
  observedCostUsd: number;
  estimatedWeeklyValueUsd: number;
  observedTokens: number;
  estimatedWeeklyTokens: number;
}

export interface CodexWeeklyQuotaData {
  quota?: CodexWeeklyQuota;
  valueEstimate?: CodexWeeklyValueEstimate;
  valueEstimateError?: string;
  error?: string;
}

export interface CursorData {
  connected: boolean;
  planType?: string;
  email?: string;
  fastUsed?: number;
  fastLimit?: number;
  percentage?: number;
  autoPercent?: number;
  apiPercent?: number;
  onDemandEnabled?: boolean;
  onDemandUsedCents?: number;
  slowUsed?: number;
  resetAt?: string;
  error?: string;
}

export interface AntigravityData {
  connected: boolean;
  status: string;
  error?: string;
}

export interface GrokProductUsage {
  product: string;
  label: string;
  usagePercent: number;
}

export interface GrokExtraCredits {
  onDemandUsedCents: number;
  onDemandCapCents: number;
  prepaidBalanceCents: number;
}

export interface GrokValueEstimate {
  observedAt: string;
  windowStartedAt: string;
  resetsAt: string;
  usedPct: number;
  observedCostUsd: number;
  estimatedPeriodValueUsd: number;
  observedTokens: number;
  estimatedPeriodTokens: number;
}

export interface GrokData {
  connected: boolean;
  planType?: string;
  email?: string;
  percentage?: number;
  resetAt?: string;
  periodStartedAt?: string;
  periodType?: string;
  periodLabel?: string;
  products: GrokProductUsage[];
  extra?: GrokExtraCredits;
  valueEstimate?: GrokValueEstimate;
  valueEstimateError?: string;
  error?: string;
}

export type CostSource = 'claude' | 'codex' | 'cursor';

export interface CostTokenBreakdown {
  inputTokens: number;
  outputTokens: number;
  reasoningTokens: number;
  cacheCreationTokens: number;
  cacheReadTokens: number;
  totalTokens: number;
}

export interface CostModelSummary {
  model: string;
  cost?: number | null;
  costUsd?: number | null;
  tokens: CostTokenBreakdown;
}

export interface CostRangeSummary {
  range: string;
  label: string;
  since?: string | null;
  until?: string | null;
  currency: string;
  cost?: number | null;
  costUsd?: number | null;
  tokens: CostTokenBreakdown;
  models: CostModelSummary[];
  validEntries: number;
  skippedEntries: number;
  elapsedMs: number;
}

export interface CostOverview {
  source: string;
  displayName: string;
  currency: string;
  generatedAt: string;
  cached: boolean;
  ranges: CostRangeSummary[];
}

export interface CostDailyPoint {
  date: string;
  cost?: number | null;
  costUsd?: number | null;
  totalTokens: number;
}

export interface CostDailySeries {
  source: string;
  currency: string;
  generatedAt: string;
  cached: boolean;
  days: CostDailyPoint[];
}
