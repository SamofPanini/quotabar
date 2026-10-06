#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP_SRC="${QUOTABAR_APP_SRC:-$ROOT/src-tauri/target/release/bundle/macos/QuotaBar.app}"
# shellcheck source=app_paths.sh
source "$ROOT/scripts/app_paths.sh"
validate_app_source

if [[ ! -x "$APP_SRC/Contents/MacOS/quotabar" ]]; then
  echo "App bundle not found. Build it first: npm run tauri build -- --bundles app" >&2
  exit 1
fi

STAGE="$(mktemp -d "$(dirname "$APP_DST")/.quotabar-install.XXXXXX")"
cleanup() {
  if [[ -e "$STAGE/previous.app" ]]; then
    echo "Previous app retained at $STAGE/previous.app" >&2
    return
  fi
  rm -rf "$STAGE"
}
trap cleanup EXIT

if ditto "$APP_SRC" "$STAGE/QuotaBar.app"; then
  :
else
  status=$?
  exit "$status"
fi

if "$ROOT/scripts/stop_app.sh"; then
  :
else
  status=$?
  exit "$status"
fi

if [[ -e "$APP_DST" ]]; then
  if mv "$APP_DST" "$STAGE/previous.app"; then
    :
  else
    status=$?
    exit "$status"
  fi
fi

if mv "$STAGE/QuotaBar.app" "$APP_DST"; then
  rm -rf "$STAGE/previous.app"
else
  status=$?
  if [[ -e "$STAGE/previous.app" ]]; then
    if mv "$STAGE/previous.app" "$APP_DST"; then
      :
    else
      echo "Failed to restore the previous app." >&2
    fi
  fi
  exit "$status"
fi

echo "Installed: $APP_DST"
