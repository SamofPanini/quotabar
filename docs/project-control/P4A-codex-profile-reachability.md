# P4A — Codex profile production reachability

Status: Authorized / implementation in progress  
Base: `main` at `80bdebb585e510015753be92d638539638d141da`  
Branch: `feat/codex-profile-reachability`  
Implementation: Terra Medium  
Acceptance: Sol High after PR and terminal CI  
Phase stop: PR + CI + acceptance. Do not merge or start a later phase automatically.

## Goal

Make the accepted P2 multi-profile backend reachable through a supported production path while preserving current single-account behavior.

The local configuration file and Rust backend registry are the source of truth. The frontend receives only safe aliases and normalized status/quota data. It must never receive, persist, or display full profile paths.

## Required architecture

1. Add a versioned QuotaBar-owned `codex-profiles.json` in the Tauri app-config directory.
2. The default Codex profile remains implicit and continues through the existing single-account commands and presentation.
3. The config lists custom profiles only:

```json
{
  "version": 1,
  "profiles": [
    {
      "alias": "profile-a",
      "home": "/absolute/path/to/custom/CODEX_HOME"
    }
  ]
}
```

Examples and tests must use synthetic paths and identities.

4. Only Rust reads and canonicalizes `home`. Reload the registry during existing automatic/manual profile refresh so config changes do not require credential migration or a separate watcher.
5. Replace the production IPC shape that accepts `Vec<CodexProfileInput>` from the webview. The production command accepts no profile list or path from the frontend.
6. Return a dedicated public DTO containing only what the UI needs:
   - safe alias;
   - connected/offline/stale/error state;
   - plan type;
   - normalized rate-limit windows and reset-credit count.
7. The public DTO and frontend boundary must not expose `home`, canonical route key, config path, account ID, email, JWT/token fields, or private paths in errors.
8. Missing config is a normal empty-custom-profile state. Malformed configuration produces a sanitized registry error and must not suppress the existing default account.
9. Preserve P2 sequential per-profile fetching and immutable per-profile credential snapshots. Do not add unbounded concurrency.
10. Add a minimal Codex-panel account section:
    - existing default-account presentation remains unchanged;
    - custom rows display only safe alias and normalized quota/status;
    - no path input, file picker, account switcher, profile editor or frontend persistence;
    - if there are no custom profiles and no registry error, legacy UI remains visually and behaviorally unchanged.
11. Manual refresh and the existing minute-scale refresh reload custom profiles through the same lifecycle. Do not create another timer or polling loop.
12. Keep configuration discovery documented. UI may say profiles are managed by local QuotaBar configuration, but must not render the resolved runtime path.

## Config validation

- Require `version: 1`.
- Preserve declared order.
- Alias must be bounded, printable and safe for display; reject reserved `default` and duplicates.
- Reject missing, relative, parent-segment, nonexistent and unresolvable homes.
- Reject a custom home that physically aliases the default home and duplicate canonical routes.
- Bound the number of configured custom profiles; the implementation may use a small documented defensive cap.
- Continue valid profiles when another entry is invalid; invalid rows/errors use sanitized aliases or stable synthetic labels.

## Required tests

Rust:

- missing config gives an empty custom list and leaves default behavior unchanged;
- version/schema validation;
- valid profiles preserve order;
- invalid/reserved/duplicate alias;
- missing/unresolvable home, duplicate route and physical default-home alias rejection;
- one bad custom profile does not suppress good profiles;
- config reload observes additions/removals;
- serialized public DTO plus Debug/error output contain no path, account ID, email or credential material;
- all P1/P2 ordering, cache, auth-failure and rotation tests remain green.

Frontend:

- production IPC sends no profile list or path;
- absent config/custom list preserves the current single-account UI;
- custom rows show safe aliases and status/quota;
- one failed row does not hide successful rows;
- path, account ID and email are neither rendered nor written to localStorage;
- manual/automatic refresh uses the existing lifecycle and does not add a timer.

Canonical validation:

- `npm ci`
- `npm run release:check`
- `npm test`
- `npm run build`
- `cargo fmt --manifest-path src-tauri/Cargo.toml --check`
- `cargo check --manifest-path src-tauri/Cargo.toml`
- `cargo test --manifest-path src-tauri/Cargo.toml`

If the executor image lacks required Linux system packages, report the exact environment limitation and rely on GitHub macOS CI. Do not weaken tests.

## Hard exclusions

Do not implement:

- Claude bridge/runtime/transport, Claude login or multi-instance support;
- Account Manager, account switching, credential migration/copy/symlink;
- frontend path entry, path display or path persistence;
- frontend redesign, tray redesign or broad settings work;
- background watcher/daemon, telemetry or an unnecessary dependency;
- P4B/P5, release soak, sleep/network-recovery testing.

## PR handoff

The PR body must include:

- exact base/head SHA and changed files;
- config schema and discovery behavior;
- public/private DTO boundary;
- backward compatibility with a missing config;
- polling/concurrency/performance impact;
- security audit proving path/account/credential non-disclosure;
- exact tests and CI URL/status;
- explicit macOS 12 local handoff items;
- confirmation that no excluded scope or later phase was started.

After opening the PR and obtaining terminal CI status, stop. Return the PR URL, head SHA, CI URL/status, test matrix, security/scope audit and unresolved risks.
