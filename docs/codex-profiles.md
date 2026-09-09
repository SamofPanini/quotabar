# Custom Codex profiles

QuotaBar reads custom Codex credential routes from `codex-profiles.json` in its
Tauri app-config directory. The default Codex profile remains implicit and is
not listed in this file.

```json
{
  "version": 1,
  "profiles": [
    { "alias": "work", "home": "/absolute/path/to/custom/CODEX_HOME" }
  ]
}
```

`alias` is the display-safe label shown to the app. `home` is read and
canonicalized only by the Rust backend; it is never returned to the webview.
The registry is read on each custom-profile refresh, so edits take effect at
the next existing manual or automatic refresh. Missing configuration simply
means that no custom profiles are configured.

The registry accepts version 1 and at most 12 custom entries. Invalid entries
are reported using sanitized labels without blocking valid entries or the
default profile. Custom homes must be absolute, resolvable, distinct, and must
not resolve to the default Codex home.
