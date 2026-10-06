import { clampProgressValue, formatResetAt, getProgressStyle } from '../utils/quota_format';
import { parseRfc3339EpochSeconds } from '../services/ping_window';

interface QuotaCardProps {
  label: string;
  percentage: number;
  resetsIn: string;
  pace?: string | null;
  featured?: boolean;
  resetAt?: string | number;
}

function getStatusColor(percentage: number): string {
  if (percentage >= 80) return 'critical';
  if (percentage >= 50) return 'warning';
  return 'good';
}

export default function QuotaCard({ label, percentage, resetsIn, pace, featured = false, resetAt }: QuotaCardProps) {
  const status = getStatusColor(percentage);
  const resetAtEpochSeconds = typeof resetAt === 'string'
    ? parseRfc3339EpochSeconds(resetAt)
    : resetAt;
  const resetAtDate = resetAtEpochSeconds == null ? undefined : new Date(resetAtEpochSeconds * 1000);
  const resetAtIso = resetAtDate && Number.isFinite(resetAtDate.getTime()) && resetAtDate.getTime() > Date.now()
    ? resetAtDate.toISOString()
    : undefined;
  const resetAtText = resetAtIso ? formatResetAt(resetAt) : '';

  return (
    <div className={`quota-card${featured ? ' featured' : ''}`}>
      <div className="quota-header">
        <span className="quota-label">{label}</span>
        <span className="quota-percentage">{percentage}%</span>
      </div>

      <div
        className="progress-bar"
        role="progressbar"
        aria-label={`${label} usage`}
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={clampProgressValue(percentage)}
        aria-valuetext={`${Math.round(percentage)}% used`}
      >
        <div
          className={`progress-fill ${status}`}
          style={getProgressStyle(percentage)}
        />
      </div>

      <div className="quota-footer">
        <span className="reset-text">Resets in {resetsIn}</span>
        <span className="reset-at-text">
          {resetAtIso && resetAtText && <time dateTime={resetAtIso}>{resetAtText}</time>}
        </span>
      </div>

      {pace && (
        <span className={`quota-pace ${percentage >= 50 ? 'warning' : ''}`}>{pace}</span>
      )}
    </div>
  );
}
