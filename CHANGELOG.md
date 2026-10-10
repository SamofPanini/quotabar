# Changelog

All notable QuotaBar changes should be summarized here before a release is cut.

## Unreleased

- QuotaBar now watches status.claude.com and status.openai.com for the services it depends on, shows incidents in the Claude and Codex panels and on the All page, and sends a notification when an incident starts, changes, or is resolved. Turn it off under Settings → Service status.
- During an incident the Claude or Codex menu bar icon shows a red dot. Usage colors are unchanged.
- QuotaBar no longer downloads model prices at runtime. Each release ships a price snapshot taken from LiteLLM (see THIRD_PARTY_NOTICES.md); it is copied into the ccstats price cache only when that cache is missing, unreadable, or older than the snapshot.
- Codex weekly value estimates now use the local price cache instead of downloading prices.
- Cursor cost is off by default. Turn on "Cursor online cost" in Settings to read Cursor usage online with ccstats credentials.
- Refresh no longer runs claude doctor on an expired Claude Code login; it re-reads the local login and checks whether Claude Code is signed out. QuotaBar no longer runs claude doctor at all.
- Add an interface size setting (100%, 125%, 150%) that enlarges text and controls; the panel keeps its tabs and footer reachable on short screens.
- Require macOS 11 or later (the interface size setting uses the native page zoom that macOS 10.15 lacks).
- Tell apart a signed-out Claude Code from an expired login: point to claude auth login and stop offering Ping renewal. QuotaBar no longer runs claude doctor automatically; it only checks the local login status.
- Update ccstats to 0.9.1 from crates.io (current model prices and parsers) and include its version in the cost cache identity.
- Cursor cost now also uses Cursor credentials saved with the ccstats CLI (`credentials.toml`), not only the `CURSOR_API_KEY` / `CURSOR_SESSION_TOKEN` environment variables.
- Discard cached cost snapshots written by a different app version or cache schema, or stamped in the future.
- Let Claude Ping renew an expired Claude Code login.
- Position the tray popover on the display whose menu-bar icon was clicked, including mixed-scale macOS setups.
- Make macOS app install, stop, and relaunch scripts transactional and restrict stopping to the selected app bundle.
- Compact the footer status and return Ping after its first confirmation read while reporting final window confirmation in the background.
- Prevent repeated Claude quota requests after expired OAuth credentials or 429 responses.
- Show each Claude quota card's valid future reset time alongside its relative reset countdown.
- Remove webview opener permissions and route dashboard links exclusively through Rust.
- Drop a late Ping "send anyway?" confirmation when its tab or account changed while the request was in flight, so a cleared confirmation no longer reappears.
- Keep healthy Codex accounts in the menu bar when another account, account info, reset credits, or the profile registry fails; the menu bar only counts fresh reads, and stale accounts stay visible and labeled in the panel.
- Keep Codex and Claude 5-hour usage above 100% as the real value in labels and menu-bar values; progress bars and icons still fill to 100%, and usage never implies permission.
- Show unavailable or invalid Codex credit balances as n/a while preserving zero and Unlimited.
- Ignore stale tray IPC completions when updating each service's skip-cache.
- Add independent Codex and Claude menu-bar icon window selectors for weekly and 5-hour quota views.
- Add an opt-in Ping footer action for opening the selected Codex or Claude 5-hour window with one minimal CLI turn.
- Add a Settings Launch at Login toggle backed by the OS login item.
- Add All / single-service settings presets, and split monthly budgets from alerts.
- Notify at 100% used and when a Codex bonus reset is unused at 100%.
- Keep last Codex weekly value visible as Last estimate when the official week is exhausted, and make leftover bonus resets a dashboard jump.
- Drive the Cursor tray from the Cursor Models pool instead of the highest dashboard bar.
- Stop caching disconnected Cursor payloads so a transient empty body cannot pin "not connected" for 120s.
- Keep Grok pool value available when valid `turn_completed` records omit the optional usage object.
- Keep last-good Cursor usage across 5xx and 429 responses instead of flashing disconnected.
- Toggle the macOS popover from a tray click instead of hiding-on-blur then immediately showing again.
- Collapse cycling macOS tray icons by status-item length instead of removing and recreating NSStatusItems.
- Return Claude last-good quota with an error and a 15-minute age cap instead of presenting hours-old data as a fresh success.
- Keep tray-enabled providers on a 60s poll while the popover is hidden, and stop writing a wall-clock "Updated" stamp onto unchanged tray data.
- Move Claude keychain reads and tray main-thread waits off Tokio worker threads, and rotate the Claude log at 2MB.
- Persist settings and events outside React state updaters, and only clear a toast when it is still the active message.
- Saturate Codex `limit_window_seconds` when converting to minutes so huge API values cannot overflow.
- Pin Codex weekly quota and local cost summaries to one ccstats revision so pricing and dedup stay aligned.
- Detect transient OS errors by errno and error-chain source instead of `os error 24` substrings.
- Scope Codex last-good account info to the current `auth.json` stamp so a transient read cannot show the previous account.
- Scale local Grok CLI cost by the Build product share instead of the full-pool used percent, and stop treating missing product percents as zero in the official total.
- Stop Grok panel validation from comparing estimate fields that the backend copies from the same official payload.

## 0.4.1 - 2026-09-01

- Build separate macOS DMGs for Apple Silicon and Intel runners.
- Build a Linux x64 AppImage on Ubuntu 22.04.
- Read Cursor's current authenticated usage summary and show separate Cursor Models and Other Models usage.
- Require every release manifest to agree on the application version.
- Add SHA-256 manifests to every desktop artifact and an opt-in, fail-closed
  Developer ID signing and notarization path for macOS release candidates.

## 0.4.0 - 2026-08-31

- Keep enabled provider trays as independent macOS menu-bar extras instead of collapsing them into one Codex slot on macOS 26.
- Added Grok Build quota tracking: SuperGrok weekly/monthly pool, product mix, extra credits, and a per-provider tray.
- Estimate SuperGrok pool USD value from ccstats' durable per-inference ledger and cache-aware, long-context pricing, scaled by the official used percent.
- Preserve the exact official Grok billing-window timestamps and fail closed when inference pricing coverage is partial or malformed.
- Added Codex weekly API-equivalent USD and token estimates from the ccstats SDK,
  while keeping estimate failures isolated from official quota and pace data.
- Show the Codex weekly value estimate against the official 7-day window even
  when the local CLI snapshot reports 0% or lives in the primary slot.
- Preserve over-limit Cursor usage while deriving truthful reset dates, normalize only the tray IPC boundary,
  cache generated tray PNGs, and stop Overview cost work while the popover is hidden.
- Updated the application version to 0.4.0 and refreshed the credential-free browser-preview screenshot.

## 0.3.1 - 2026-08-22

- Added a validated Codex weekly quota pace projection, including projected usage at reset, risk status, and estimated depletion time from the ccstats SDK.
- Kept weekly pace failures isolated from official Codex quota data and rejected stale, cross-window, or divergent local snapshots instead of displaying misleading projections.
- Hardened concurrent Codex refreshes and account-scoped quota caching against stale or partial state.
- Made provider switcher transitions transactional and reduced unnecessary forced tray icon resynchronization.
- Reduced repeated local cost parsing with multi-range summaries and cached snapshots.
- Updated the application version to 0.3.1 and refreshed macOS and Windows release artifacts.

## 0.3.0 - 2026-07-17

- Surfaced settings storage read and write failures instead of silently degrading, including background-write failures.
- Prevented stale provider refreshes from overwriting newer state and made popover visibility failures observable and fail closed.
- Committed notification deduplication only after delivery and kept fatal frontend diagnostics safe and observable.
- Removed sensitive Claude credential fragments from diagnostics, unused Codex stats wiring, and the obsolete priority price override.
- Split legacy and redesigned stylesheets into smaller modules without changing their rendered output.
- Created the annotated v0.3.0 source tag. No GitHub Release artifacts were published for this tag.

## 0.2.0 - 2026-07-06

- Added an artifact-only GitHub Actions workflow for macOS and Windows release bundle inspection.
- Added SECURITY, CONTRIBUTING, and CODE_OF_CONDUCT files for public project readiness.
- Replaced the disabled Tauri CSP with a restricted policy for bundled UI assets and IPC.
- Clarified first-release gates, source-build fallback, and the no-auto-publish release boundary.
- Reduced local cost refresh work by using ccstats multi-range summaries for Today, This Week, and This Month in one pass.
- Added a browser-preview demo proof screenshot and documented its no-credential capture scope.
- Report an explicit desktop-backend-unavailable error when the UI is opened outside Tauri.
- Added CI coverage for frontend tests, frontend build, Rust formatting, Rust check, and Rust tests.
- Added GitHub issue templates and a pull request template.
- Documented release artifact paths and current limitations.
- Existing application version. Historical release notes were not tracked before this changelog.
