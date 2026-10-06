#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# shellcheck source=app_paths.sh
source "$ROOT/scripts/app_paths.sh"

if [[ ! -x "$APP_DST/Contents/MacOS/quotabar" ]]; then
  echo "QuotaBar is not installed at $APP_DST. Run ./scripts/install_app.sh first." >&2
  exit 1
fi

open "$APP_DST"
