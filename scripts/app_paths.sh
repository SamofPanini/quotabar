#!/usr/bin/env bash

# This file is sourced by the lifecycle scripts. It exits the caller before it
# can invoke a process or mutate an app bundle when a path is unsafe.
fail_path() {
  echo "Invalid QuotaBar app destination: $1" >&2
  exit 2
}

validate_app_destination() {
  APP_DST="${QUOTABAR_APP_DST:-/Applications/QuotaBar.app}"
  [[ "$APP_DST" != */ ]] || fail_path "must not end with a slash"
  [[ "$APP_DST" = /* ]] || fail_path "must be an absolute path"
  [[ "$(basename "$APP_DST")" == "QuotaBar.app" ]] || fail_path "basename must be QuotaBar.app"

  local parent
  parent="$(dirname "$APP_DST")"
  [[ -d "$parent" ]] || fail_path "parent directory does not exist: $parent"
  if [[ -e "$APP_DST" || -L "$APP_DST" ]]; then
    [[ ! -L "$APP_DST" ]] || fail_path "destination must not be a symbolic link"
    [[ -d "$APP_DST" ]] || fail_path "destination must be a directory"
  fi
}

validate_app_source() {
  [[ "$APP_SRC" = /* ]] || {
    echo "Invalid QuotaBar app source: must be an absolute path" >&2
    exit 2
  }
}

escape_ere() {
  # Escape every POSIX ERE metacharacter before interpolating a filesystem path.
  printf '%s' "$1" | sed 's/[][\\.^$*+?(){}|]/\\&/g'
}

validate_app_destination
APP_PROCESS_PATTERN="^$(escape_ere "$APP_DST")/Contents/MacOS/quotabar([[:space:]].*)?$"
