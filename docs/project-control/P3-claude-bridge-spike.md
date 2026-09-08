# P3 Claude bridge spike

## Dispatch boundary

- **Base:** after this brief is merged, the dispatcher must resolve and state the exact `main` SHA that contains both project-control documents. The implementer must verify that SHA before writing; do not branch from the pre-document P2 SHA.
- **Implementation branch:** `feat/claude-bridge-spike`
- **Objective:** a contract/parser plus mock-transport spike that emits one paid-like and one free-like normalized snapshot.
- **Not an objective:** real login, a real provider transport, a local listener/IPC, or UI wiring.

The current Claude path is a legacy single `get_quota` command that calls `claude::fetch_quota`; it reads environment/Keychain OAuth state and usage data. That path must remain behavior-compatible and untouched by this spike.

## Protocol v1

The internal normalized envelope is typed and strict. It is test-only in P3: no new IPC or public API. It contains only:

```text
version: "v1"
instanceId: string
route: { kind: "claude_desktop" | "claude_web" | "mock"; label?: synthetic/app-owned string }
snapshot: {
  source: "internal_usage_api" | "completion_sse" | "local_estimate" | "stale_cache"
  confidence: "verified_server" | "observed" | "estimated" | "unknown"
  observedAt: RFC 3339 timestamp
  usageWindows?: [{ name, usedPercent?, resetAt? }]
}
error?: { code: "unavailable" | "malformed_payload" | "unsupported_route" | "redacted_field" | "stale"; retryable: boolean }
```

`instanceId` and route metadata are app-owned routing labels, not provider identities. `route.label` may contain only a synthetic/app-owned label and must not carry provider-derived identity. The internal envelope and every log line must exclude organization ID, email, provider account ID, token, cookie, session key, authorization value, and raw provider payload. The protocol validator must fail closed on unknown or forbidden fields; it must reject (or redact before construction) forbidden keys and values before a result can leave the adapter.

Usage windows are optional. An empty free `/usage` response means “no endpoint usage window”, not zero use and not an error by itself. Source precedence is:

```text
valid endpoint > completion SSE > local estimate > stale cache
```

Exception: an empty free endpoint permits completion SSE to populate the normalized snapshot. A malformed endpoint does not become valid merely because it is empty.

## Design constraints

- Mock transport is an internal test seam only; do not open a localhost listener or add IPC without separate approval.
- Store snapshots by `instanceId`; no shared mutable fallback may leak a paid-like snapshot into a free-like instance or the reverse.
- Preserve the existing legacy `get_quota` command and `claude::fetch_quota` behavior. Do not redirect them through the spike.
- Fail closed: absent or malformed data remains absent/explicitly unavailable, never a fabricated `0%`.

## Proposed allowlist

The implementer may add only the minimum files required by the existing module layout, expected to be limited to:

```text
src-tauri/src/domain/models.rs
src-tauri/src/services/claude_bridge.rs
src-tauri/src/services/mod.rs
src-tauri/src/services/claude_bridge_tests.rs (or inline unit tests)
tests/claude_bridge_contract.test.ts (only if a frontend-independent contract fixture check is needed)
docs/project-control/P3-claude-bridge-spike.md
```

If `commands.rs`, `services/claude.rs`, frontend files, dependencies, lockfiles, CI, or any credential-reading code must change, stop and request a new scope decision. The final PR must list actual files versus this allowlist.

## Fixture contract

Create synthetic, secret-free fixtures only:

- `paid-like`: a valid endpoint-shaped payload normalizes to `internal_usage_api` / `verified_server` with at least one usage window.
- `free-like`: an empty endpoint-shaped `/usage` payload plus a completion-SSE fixture normalizes to `completion_sse` / `observed`; no endpoint window is invented.
- `stale-like`: a cached fixture normalizes to `stale_cache` / `unknown` (or the documented lower confidence chosen by the implementation) and carries a safe `stale` error where appropriate.
- `redaction`: forbidden keys and sentinel values are both rejected/redacted; neither appears in normalized output, debug formatting, or safe errors.

No fixture contains a real account, organization, email, cookie, OAuth token, session key, authorization header, or raw captured payload.

## Product-to-test mapping

| Product requirement | Required test |
| --- | --- |
| Typed v1 envelope | Accept valid fixture; reject malformed version/source/confidence/window shape. |
| Paid-like snapshot | Endpoint fixture produces `internal_usage_api`, `verified_server`, and its own instance store entry. |
| Free empty `/usage` semantics | Empty endpoint plus SSE produces SSE data; empty endpoint alone does not fabricate a window or zero. |
| Precedence | Endpoint wins over SSE/estimate/cache; empty free endpoint allows SSE; malformed endpoint fails closed. |
| Instance isolation | Alternating paid/free fixture writes and reads never cross instances. |
| Credential safety | Validator fails closed on unknown/forbidden fields; safe error/log formatter excludes every forbidden key and sentinel value. |
| Legacy compatibility | Existing Claude tests and direct `get_quota` path remain unchanged and pass. |

## Verification

Run the canonical gate from the repository root:

```bash
npm ci
npm run release:check
npm test
npm run build
cargo fmt --manifest-path src-tauri/Cargo.toml --check
cargo check --manifest-path src-tauri/Cargo.toml
cargo test --manifest-path src-tauri/Cargo.toml
```

Add focused bridge tests to the relevant frontend or Rust command above; do not introduce a new external service as a test dependency.

## PR body checklist

- [ ] Base is the exact dispatcher-provided `main` SHA containing both project-control documents (or a separately approved rebase), and that SHA is recorded in the PR.
- [ ] Scope is parser/normalizer plus synthetic mock transport only.
- [ ] Actual changed files match the allowlist, or deviations are explained and approved.
- [ ] `get_quota` and `claude::fetch_quota` are behavior-compatible and not rerouted.
- [ ] Paid-like and free-like fixtures, precedence, isolation, and redaction tests pass.
- [ ] All seven canonical commands pass.
- [ ] No secret, identity, raw payload, listener, IPC, UI, or real network transport was added.
- [ ] Local-only validation remains explicitly pending.

## Acceptance and stop conditions

Accept only when the strict envelope, precedence, redaction, synthetic fixtures, isolation, and legacy regression checks all pass within scope. Stop when a real authenticated request, captured payload, new credential reader, browser/desktop injection, localhost listener, IPC surface, UI change, dependency addition, or account identity is needed. Those are later design decisions, not hidden implementation details.

## Local handoff

After cloud acceptance, a local owner may decide whether to prototype a real bridge. Before that work, define consent, credential ownership, process isolation, desktop/runtime compatibility on macOS 12 Intel, redacted observability, and a manual login/logout/rotation test plan. Do not use live data until those decisions are approved.

## Mechanism references

[Claude-WebExtension-Launcher](https://github.com/lugia19/Claude-WebExtension-Launcher) and [Claude-Usage-Extension](https://github.com/lugia19/Claude-Usage-Extension) are GPL-3.0 mechanism references only. Do not copy code, vendor assets, or add a runtime dependency; this spike is an independently designed, test-fixture-only contract.
