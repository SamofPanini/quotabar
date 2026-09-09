# P4B Claude paid/free acquisition gate

## Control status

- **Project:** `SamofPanini/quotabar`
- **Authoritative base:** `91c4137bcabca8aadb4e9f4c3649efdd624d57b5`
- **Control document branch:** `docs/p4b-claude-acquisition-gate`
- **Implementation branch after approval:** `spike/claude-paid-free-acquisition`
- **Objective:** prove that one already-authenticated paid Claude web account and one already-authenticated free Claude web account can each yield a sanitized quota snapshot without exporting or persisting their credentials.
- **Stop:** this document does not authorize P4C account tabs, P4D four-account rollout, a production localhost bridge, Claude account switching, login automation, or credential migration.

P4A-R is accepted, merged, and locally passed. P4B is the next dependency because P4C cannot truthfully render Claude account tabs until the paid and free acquisition semantics are proven.

## Evidence-based mechanism decision

### Supported facts

- Anthropic documents Claude Code credentials separately from Claude Desktop/cloud sessions. Claude Desktop does not read Claude Code's environment/helper credential path.
- Paid Claude plans expose five-hour and weekly usage in Claude Settings > Usage. Paid Claude and Claude Code activity share the same plan limits.
- Anthropic does not document a supported third-party API for reading the active Claude Desktop session or its usage state.
- The current QuotaBar production Claude implementation therefore remains a Claude Code OAuth compatibility path; it cannot be relabeled as Claude.app/Web support.

### Reference-code findings

Pinned reference revisions:

- `lugia19/Claude-WebExtension-Launcher@5794d0e55f8d3496ebac9027604d7f74e62d020d`
- `lugia19/Claude-Usage-Extension@e46646edf74400b5fab697862d0abfe7f69c9066`

The launcher demonstrates that isolated Desktop instances and extensions are technically possible, but it is an unofficial patched client. Its own documentation identifies macOS notarization, first-launch network/Keychain, and Cowork/signature limitations. It is not an acceptable production dependency for a stability-first QuotaBar path.

The usage extension demonstrates two acquisition mechanisms:

1. paid-like data from same-origin `/api/organizations/<route>/usage`;
2. free-like 5h/7d windows from the completion SSE `message_limit.windows` payload when the free `/usage` result is empty.

It is GPL-3.0 and must remain a mechanism reference only. Do not copy code or assets into this MIT repository. Its published privacy policy also states that organization ID and usage statistics are synchronized through Firebase. Installing or depending on it for this project's live validation would violate QuotaBar's provider-identity boundary.

### Selected route for P4B

Use an independently authored, temporary, local-only Chromium extension probe against two isolated, already authenticated `claude.ai` browser profiles.

This is preferred over patching Claude Desktop because it:

- keeps the browser as credential owner;
- requires no cookie/session/token export;
- avoids modifying or re-signing Claude.app;
- avoids the third-party extension's Firebase path;
- can be removed completely after the acquisition proof;
- isolates acquisition feasibility from QuotaBar transport and UI design.

**Claude.app-only remains unsupported in P4B.** If the two accounts cannot be made available in isolated browser profiles, stop with `BLOCKED / AUTHENTICATED_WEB_CONTEXT_UNAVAILABLE`. Do not inspect, decrypt, copy, or replay Claude.app Electron storage, cookies, Local Storage, IndexedDB, Keychain items, session keys, or authorization values.

## P4B split

### P4B-1 — cloud implementation and review of a local probe

Terra Medium may implement only the temporary probe and its synthetic tests on `spike/claude-paid-free-acquisition`. Sol High must review it before any live account is used.

Expected location:

```text
tools/local-validation/claude-acquisition-probe/
docs/project-control/P4B-claude-paid-free-acquisition.md
```

The probe is not bundled into QuotaBar and is not a production dependency.

### P4B-2 — macOS 12 Intel live acquisition proof

After Sol High acceptance, local Terra Medium may load the unpacked probe into two isolated browser profiles and validate one paid account plus one free account. This is the first live P4B gate.

No performance matrix is required. Measure only acquisition latency and obvious sustained CPU/RSS behavior if the probe itself visibly misbehaves. Do not repeat P4A-R release, idle, or full regression tests.

### P4B-3 — control-plane decision

The Work control plane reviews the two sanitized snapshots and decides whether to authorize a separate production transport phase. Do not add listener, native messaging, Tauri IPC, persistence, tray, or tabs during P4B-1/2.

## Probe contract

### Acquisition

The probe may observe only these same-origin response classes:

- `GET /api/organizations/<opaque-route>/usage`
- completion/retry-completion SSE events containing `type: "message_limit"`

The opaque route segment may exist transiently inside the page-world acquisition function only. It must never cross into the content script, extension service worker, popup, storage, logs, test output, screenshots, or Notion/GitHub evidence.

For completion SSE:

- parse only the `message_limit` event;
- do not accumulate assistant text, thinking summaries, tool input, conversation payloads, or other SSE records;
- cap parser memory and fail closed on overflow or malformed input;
- never delay, modify, cancel, retry, or otherwise affect the response consumed by Claude.

### Sanitized output

The probe emits exactly:

```text
version
probeSlot          // "profile-a" | "profile-b", assigned locally
planClass          // "paid" | "free" | "unknown"
source             // "usage_endpoint" | "completion_sse"
observedAt
windows[]          // { kind: "five_hour" | "weekly", usedPercent?, resetAt? }
status              // "available" | "unavailable" | "malformed"
errorCode?          // fixed enum only
```

It must not emit or retain:

- organization/account/user identifiers;
- email, display name, plan product name tied to identity;
- URL/path containing the route identifier;
- cookies, tokens, session keys, authorization headers;
- request headers/bodies or raw provider responses;
- conversation ID, message ID, request/trace ID;
- conversation text, uploaded data, tool data;
- arbitrary provider error text.

Unknown fields fail closed. Missing windows remain missing and never become zero.

### Storage and network

- No Firebase, analytics, telemetry, update service, remote log, or non-Claude network host.
- No `<all_urls>`; host permission is limited to `https://claude.ai/*`.
- No credential, provider identity, raw response, or conversation content in `chrome.storage.local`, IndexedDB, filesystem, clipboard, console, or crash output.
- Prefer in-memory/`chrome.storage.session` state. Clearing the probe or closing the browser must be sufficient to remove the observation.
- The probe must not expose a localhost server, fixed port, native messaging host, or externally callable bridge.

## Required synthetic tests

- paid endpoint fixture produces five-hour and weekly windows.
- free empty endpoint does not fabricate zero or a window.
- free SSE fixture produces only permitted five-hour/weekly fields.
- paid endpoint data outranks SSE for the same observation cycle.
- malformed/unknown fields fail closed.
- route, organization ID, email, token, cookie, authorization, conversation text, message/request IDs, and sentinel values cannot appear in output, Debug/console formatting, storage serialization, or errors.
- SSE parser ignores all events except `message_limit` and enforces its memory cap.
- interception returns the original response path without mutation or blocking.
- two synthetic slots never overwrite or inherit from one another.
- repository production build and existing QuotaBar behavior remain unchanged.

## Canonical cloud gate

Run from the repository root:

```bash
npm ci
npm run release:check
npm test
npm run build
cargo fmt --manifest-path src-tauri/Cargo.toml --check
cargo check --manifest-path src-tauri/Cargo.toml
cargo test --manifest-path src-tauri/Cargo.toml
```

Also run the probe's focused tests using only existing toolchain dependencies. A new dependency or lockfile change is a stop condition unless separately approved.

## Sol High acceptance gate

Reject P4B-1 if any of the following is true:

- copied GPL implementation or assets;
- third-party runtime dependency;
- provider identity/raw payload can cross the page-world boundary;
- assistant/conversation content is buffered or parsed;
- probe can write or alter Claude requests/session state;
- network permission exceeds `claude.ai`;
- persistent storage contains live observations;
- a listener/IPC/native host/production QuotaBar path was introduced;
- source scope extends into P4C/P4D.

Accept only after reading the exact diff, verifying independent implementation, running focused security tests, and confirming all canonical checks.

## Local live gate

Use synthetic ledger labels `profile-a` and `profile-b`. Never record actual Chrome profile names or private paths.

For each slot record only:

- paid/free/unknown;
- endpoint or SSE source;
- available window kinds, percentages, and reset timestamps;
- acquisition latency;
- safe status/error enum;
- whether browser login remained intact;
- whether probe storage was cleared and extension removed.

Paid success requires a real endpoint-derived five-hour window and at least one weekly window. Free success requires either a real SSE-derived five-hour window or an explicit `UNAVAILABLE` result proving that current free responses contain no usable window. The free account may require one user-authorized ordinary Claude message; Terra must not send a message or spend quota without that explicit authorization.

Do not claim that a free account provides stable polling: SSE acquisition is event-driven and can be unavailable until a completion or rejected send occurs.

## Phase outcomes

- `PASS`: paid endpoint snapshot plus free SSE snapshot are both obtained and sanitized.
- `PASS_WITH_LIMITATION`: paid succeeds; free is safely unavailable or event-triggered only, with no fabricated usage.
- `BLOCKED`: authenticated browser contexts are unavailable or the probe cannot meet the credential/identity boundary.
- `FAIL`: cross-account leakage, provider identity exposure, request mutation, credential/session disturbance, or unacceptable runtime behavior.

Any outcome returns to the Work control plane. Never continue automatically to a production bridge, P4C, or P4D.

## Sources

- Anthropic Claude Code authentication: https://code.claude.com/docs/en/authentication
- Anthropic usage-limit guidance: https://support.claude.com/en/articles/9797557-usage-limit-best-practices
- Anthropic Pro/Max and Claude Code shared limits: https://support.claude.com/en/articles/11145838-use-claude-code-with-your-pro-or-max-plan
- Mechanism reference only: https://github.com/lugia19/Claude-WebExtension-Launcher
- Mechanism reference only: https://github.com/lugia19/Claude-Usage-Extension
