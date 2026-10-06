#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# shellcheck source=app_paths.sh
source "$ROOT/scripts/app_paths.sh"

"$ROOT/scripts/install_app.sh"
"$ROOT/scripts/run_app.sh"

echo "Reinstalled QuotaBar and requested launch."
