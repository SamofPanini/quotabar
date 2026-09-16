# P4C Claude production bridge ADR

## Status, decision, and scope

- **Status:** revised proposal; Stream C is blocked pending fresh Sol High approval and reservation of the two Chrome Web Store (CWS) item IDs.
- **Base:** `e3e0e6729c382577dacf9b0206da3cf47c4e113e`.
- **Scope:** architecture only. This ADR authorizes no extension, host, manifest, Rust/Tauri/frontend code, authenticated browser load, Claude message, or installer change.

The selected production route is:

```text
Claude page MAIN world -> bounded parser/sanitizer -> isolated extension context
  -> Chrome Native Messaging -> QuotaBar current-only two-slot store -> read-only IPC
```

There is no localhost listener, polling, heartbeat, provider-request origination, P4B probe promotion, P3 `claude_bridge.rs` promotion, or `get_quota` reroute. The existing Claude Code path remains unchanged.

## Threat model and trust boundary

Chrome owns browser sessions. No component may read/export a cookie, token, session key, password, profile path, email, account/organization/provider ID, URL route segment, headers, bodies, raw SSE, conversation content, or arbitrary provider text.

Chrome Native Messaging admits an allowlisted extension origin, but that origin is not a per-profile identity. The host therefore validates both Chrome's first argv origin and a durable, app-owned per-extension-install identity plus proof. A binding key is random local authentication material, not a Claude credential, and is stored only in that Chrome profile's extension storage. It cannot prove security against a malicious process running as the same macOS user that can read the key or execute/replace the host; that same-user compromise is explicitly out of scope. Host/manifest/binary and state paths must nevertheless be owner-controlled to prevent a lower-privilege or other-user replacement.

The page MAIN world is untrusted. It may forge an advisory usage candidate; the bridge result must never drive billing, authentication, authorization, execution, account switching, credential migration, or another security decision. It is display-only quota telemetry.

## Wire protocol v1

All input is UTF-8 JSON, parsed with duplicate-key rejection and exact-object validation: unknown fields are rejected, absent fields are not defaulted, `type` is the discriminator, objects have no prototype/inherited fields, and no value is logged. Every string is at most 64 UTF-8 bytes except RFC 3339 timestamps (40) and `proof` (88 base64url bytes). Messages are at most 8 KiB; arrays are at most two entries; nesting is at most two levels.

Native Messaging itself is JSON prefixed by a 32-bit **native-byte-order** length (not a fixed little-endian choice), as required by Chrome. The host rejects declared/actual lengths over 8 KiB before JSON parsing and writes no diagnostic bytes to stdout.

### Pairing unions

| Message | Required fields | Forbidden / validation |
| --- | --- | --- |
| `PairRequest` | `type:"pair_request"`, `version:"v1"`, `installId` (UUIDv4), `pairCode` (32 base64url chars), `requestNonce` (16 base64url chars) | No `slot`, key, snapshot, error, credential, identity, or extra field. Pair code is one-use, host-generated, 5-minute TTL, not persisted by the extension. |
| `PairResponse` | `type:"pair_response"`, `version:"v1"`, `slot`, `bindingId` (UUIDv4), `keyId` (UUIDv4), `bindingKey` (43 base64url chars), `nextSequence:1` | Only host response to a valid pair request. `bindingKey` crosses the native channel once, is stored in extension-local storage for that profile, never page world/log/IPC/store. |
| `UnpairRequest` | `type:"unpair_request"`, `version:"v1"`, `bindingId`, `installId`, `sequence`, `proof` | No windows/source/error. Host authenticates then transactionally invalidates binding and deletes its snapshot. |
| `UnpairResponse` | `type:"unpair_response"`, `version:"v1"`, `bindingId`, `result:"removed"` | No key, snapshot, or free text. |

`slot` is exactly `slot-a` or `slot-b`; it is an app-owned routing label, never a profile name. At most two active bindings exist. Explicit local QuotaBar UI creates a pair code for one empty slot. A second binding for an occupied slot is rejected. Re-pairing requires an explicit invalidation transaction, then a new pair code; it cannot silently replace a binding.

### Steady-state observation unions

| Message | Required fields | Forbidden / validation |
| --- | --- | --- |
| `AvailableSnapshot` | `type:"available"`, `version:"v1"`, `bindingId`, `installId`, `keyId`, `sequence` (positive u64), `source`, `observedAt`, `windows`, `proof` | `source` is `completion_sse` or `usage_endpoint`; `windows` has 1–2 unique kinds. No status/error/pair fields. |
| `UnavailableObservation` | `type:"unavailable"`, `version:"v1"`, `bindingId`, `installId`, `keyId`, `sequence`, `observedAt`, `errorCode`, `proof` | No windows/source/status/pair fields. `errorCode` is exactly `unavailable`, `malformed_payload`, or `unsupported_observation`. |

A window is an exact object with `kind` (`five_hour` or `weekly`), optional `usedPercent` finite 0–100, and optional `resetAt` RFC 3339 UTC. Duplicate kinds, an empty window array, invalid/future timestamps, and unknown properties are rejected. Missing usage is never converted to zero.

`proof` is `base64url(HMAC-SHA-256(bindingKey, canonical-json(message-without-proof)))`. Canonical JSON is UTF-8, lexicographically sorted keys, no whitespace, escaped per RFC 8785-style JSON string serialization, integers only for `sequence`, and the exact array order shown. The implementation must ship a single shared test-vector fixture. The host compares proof in constant time.

Each binding has a host-held `nextSequence`. A message is accepted only when its sequence equals that value; on atomic acceptance the host increments it. Lower, duplicate, skipped, or rollback sequence is rejected without store mutation. Key rotation is an explicit authenticated host response containing a new `keyId`, random key, and reset `nextSequence:1`; host atomically replaces the key and sequence, and the extension replaces the old key only after validating that response. Lost extension state or a failed rotation requires explicit unpair/re-pair, not a fallback binding.

## Acquisition and validation boundary

Raw fetch/SSE bytes exist only in the MAIN/page world. That code has a fixed 64 KiB parser cap, observes only the authorized same-origin response classes, extracts only `message_limit` windows, and neither buffers other events nor changes the response path. It constructs a strict sanitized candidate; raw payload and route identifiers never cross `window.postMessage`.

The isolated content-script bridge accepts only the exact candidate origin/source/type/shape, repeats all bounds and forbidden-field validation, and sends the signed union through the extension service worker. The host repeats schema, binding, HMAC, sequence, timestamp, and source-precedence validation before writing. `host_unavailable` is extension-local ephemeral state when the host cannot be reached: it is never a provider observation, never sent later, and never stored as history.

## Host time, TTL, and precedence

The host stamps `receivedAt` after successful authentication. It accepts `observedAt` only if it is no more than 5 minutes before or 60 seconds after host receipt and is strictly later than that binding's last accepted observation; otherwise it rejects the message. Freshness is measured exclusively from `receivedAt` with a 15-minute TTL.

| Current state | New authenticated event | Result |
| --- | --- | --- |
| no unexpired snapshot | fresh valid SSE | write SSE snapshot |
| no unexpired snapshot | fresh valid endpoint | write endpoint snapshot |
| unexpired valid endpoint | fresh valid SSE | atomically replace with SSE |
| unexpired valid SSE | fresh valid endpoint | keep SSE; consume sequence but do not replace state |
| unexpired valid SSE | invalid/unavailable/replayed event | keep last-good SSE; record only current safe metadata |
| expired SSE | fresh valid endpoint | endpoint may replace expired state |
| any snapshot | host-time TTL elapsed | IPC returns `expired` and no windows; store may retain current record only for validation then deletes it on next successful write/startup sweep |

IPC may return last-good windows with a fixed `lastErrorCode` while still unexpired; it may never return free text. Expiry returns no windows. No source failure erases last-good before TTL.

## Current-only atomic state

Production and validation each use a separate private `0700` root. Each root contains only a binding record and up to two current snapshot records; no logs, history, backups, crash copies, or raw payload. Every final file is an owner-owned regular file, `0600`; all opens use no-follow semantics and verify owner, mode, schema/version, expected inode type, and bounded size before use.

For a write, acquire a root-local exclusive lock; serialize bounded data to a same-directory `0600` temp created with `O_CREAT|O_EXCL|O_NOFOLLOW`; `fsync(temp)`; atomically rename it over the final record; then `fsync(root directory)`; release lock. Startup validates all records before serving, deletes only owned invalid/orphan temp files within the same root, and never restores from backup. Pair invalidation/re-pair obtains the same lock and commits binding removal plus snapshot removal as one journaled replace transaction; crash recovery chooses only a complete committed state, otherwise removes both. “Removal” means deletion with no backup, not a claim of secure erase.

## Distribution, support, and namespace isolation

| Surface | Production | Validation candidate | Hard-fail rule |
| --- | --- | --- | --- |
| CWS item / extension ID | Reserved production CWS item and immutable ID (required before C) | Separate reserved validation CWS item and ID | Installer refuses a missing, equal, unpacked, or developer ID. |
| App identity | Production QuotaBar bundle/signing/receipt identity | Separate bundle/signing/receipt identity | Any receipt or bundle collision aborts before mutation. |
| Native host | Unique production host name, manifest, binary | Different name, manifest, binary | Existing manifest/name pointing across namespace aborts. |
| State | Production `0700` root | Different `0700` root | Canonicalized roots must differ; symlink/path collision aborts. |
| Update/rollback/uninstall | Update only matching signed receipt; rollback validates schema | Same, inside validation namespace | Uninstaller removes only verified own host/manifest/binary/root; mismatch aborts. |

Google Chrome production distribution is CWS-only on macOS: external macOS installation metadata may point only to the CWS update URL. The production floor is a maintained OS and Chrome stable release, initially **macOS 13+ with then-supported Chrome stable**; exact minimum version is a release decision. macOS 12 is not a production floor: it is a legacy compatibility target frozen at Chrome 150, the final Monterey Chrome version. It carries unpatched-browser risk and may be used only for separately approved validation, never as evidence that current stable is supported. Stream C cannot begin until both CWS IDs and this floor are recorded in a release-controlled decision.

## Lifecycle and recovery

| Event | Required behavior |
| --- | --- |
| Install | Installer validates signed receipt, all unique names/roots/IDs, host manifest path and owner/mode, then installs; any collision hard-fails with no partial fallback. |
| Chrome/extension restart or sleep/wake | No heartbeat/poll. Last-good is readable only to host-time TTL; next real observation may update. |
| QuotaBar restart | Validate owner/mode/schema/lock state before IPC; invalid state is deleted and returns unavailable. |
| Missing host/network | No provider request/retry loop. Extension exposes ephemeral `host_unavailable`; current record ages normally. |
| Upgrade | Preserve binding only if CWS ID, schema, signature, and namespace match; otherwise transactionally invalidate and require re-pair. |
| Rollback | Accept only compatible signed predecessor; otherwise invalidate binding and remove snapshots. |
| Uninstall | Verify own receipt and canonical namespace, then remove only own host/manifest/binary/root and CWS external-install metadata; never remove the other namespace. |

## Evidence appendix

### Pairing sequence

```text
QuotaBar: create one-use pairCode for empty slot
Extension install: random installId -> PairRequest(pairCode, nonce)
Chrome -> host: argv origin checked; host checks CWS ID, code, empty slot
host: atomically creates binding {slot,bindingId,installId,keyId,key,nextSequence=1}
host -> extension: PairResponse(binding key once)
extension: stores key locally in this Chrome profile; no page-world exposure
```

### Authenticated observation sequence

```text
MAIN: bounded raw parse -> sanitized candidate
isolated extension: revalidate -> sequence N + HMAC
Chrome -> host: argv origin + schema + binding + HMAC + N + host-time checks
host: apply precedence, atomically replace current record if eligible, nextSequence=N+1
IPC: read-only sanitized current state
```

### Official platform evidence

| Claim | Evidence |
| --- | --- |
| Native host uses stdio; manifest has fixed `allowed_origins`; Chrome supplies caller origin; messages use native-byte-order framing. | [Chrome Native Messaging](https://developer.chrome.com/docs/extensions/develop/concepts/native-messaging) |
| macOS external extension installation must use a Chrome Web Store update URL and exposes the CWS extension ID. | [Chrome extension distribution](https://developer.chrome.com/docs/extensions/how-to/distribute/install-extensions) |
| Monterey support ends with Chrome 150 in mid-2026. | [Chrome support announcement](https://support.google.com/chrome/thread/404150391/sunsetting-support-for-macos-12-monterey-in-mid-2026) |

## Acceptance matrix and remaining gate

Implementation must demonstrate exact-union/duplicate-key caps, pair/rotate/unpair/replay rejection, two-profile binding isolation, MAIN-to-isolated sanitization, host-time TTL and precedence, no local listener/polling, safe-error coexistence, atomic interruption/corruption behavior, and hard-fail production/validation isolation. It must preserve `get_quota` unchanged. Sol High must review the exact ADR diff and approve the reserved CWS IDs, maintained production floor, same-user boundary, pairing proof, atomic layout, and live-validation consent plan before Stream C.
