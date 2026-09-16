# P4C Claude production bridge ADR

## Status and decision

- **Status:** proposed; implementation is blocked pending Sol High design and security approval.
- **Authoritative base:** `e3e0e6729c382577dacf9b0206da3cf47c4e113e`.
- **Scope:** production architecture only. This ADR does not authorize an extension, native host, manifest, Tauri command, persistence implementation, authenticated-profile loading, Claude message, or UI change.

### Decision

QuotaBar will acquire Claude Web usage through an independently authored Chrome extension, delivered through Chrome Native Messaging to a QuotaBar-owned, sanitized current-snapshot store, and expose that store only through a read-only Tauri IPC command.

```text
authenticated Chrome profile
  -> app-owned extension (same-origin observation and sanitization)
  -> Native Messaging host (framed, schema-validated ingress)
  -> QuotaBar private current-snapshot store (two app-owned slots)
  -> read-only Tauri IPC
  -> later shared Claude panel/tray work
```

This is deliberately not a localhost design. No listener, port, WebSocket, HTTP endpoint, or provider polling is permitted. If Native Messaging cannot satisfy the distribution and stable-ID decisions below, return to the control plane with the specific feasibility evidence; do not substitute localhost transport.

## Context and non-goals

P4B established a local, temporary, browser-only acquisition probe. Its generated probe and P3's `src-tauri/src/services/claude_bridge.rs` are not production starting points. The P3 module is test-only and encodes endpoint-first selection, while the production rule below requires valid completion SSE to take precedence.

The existing Claude Code `get_quota` / `claude::fetch_quota` compatibility path remains untouched. It is separate from authenticated browser-profile acquisition and must not be relabeled or rerouted as this bridge.

This ADR excludes account switching, login/logout, credential migration, execution dispatch, Claude tabs, tray aggregation, provider identity display, analytics, history, cloud synchronization, and any use of the temporary probe in an installed build.

## Security and privacy invariants

| Boundary | Required invariant | Reject or degrade safely when violated |
| --- | --- | --- |
| Browser profile | Chrome retains the authenticated session; QuotaBar and the host receive neither profile path nor provider identity. | Pairing remains unbound/unavailable; never inspect browser storage or credentials. |
| Extension observation | Observe only real, same-origin Claude responses needed for a sanitized usage snapshot. It must not send heartbeats, poll provider endpoints, originate provider requests, change responses, or buffer conversation content. | Emit no envelope, or a fixed safe error enum. |
| Extension output | No cookie, token, session key, authorization value, email, display name, account/organization/provider ID, URL/route identifier, headers, bodies, raw SSE, conversation text, or arbitrary provider error text crosses the page/extension boundary. | Fail closed before Native Messaging. |
| Native host | Accept only an installed, stable extension ID; accept bounded native frames, strict schema/version/size, and fixed app-owned slots. | Drop the frame, retain last valid current snapshot until TTL, and record only a fixed safe error. |
| Local store | Keep at most two current sanitized snapshots, atomically replaced, file mode `0600`, with no raw payload and no history. | Expose per-slot unavailable state; never fabricate zero use. |
| Tauri IPC | Read-only sanitized DTO only; no path, slot configuration, pairing, credential, host-control, or raw-payload command. | Command returns fixed safe state only. |

No component may derive a provider identity from the profile, Chrome path, page URL, OAuth state, plan-product text, or data not explicitly allowed by the schema.

## Production protocol

### App-owned slots and pairing

The store has exactly two fixed opaque slots: `slot-a` and `slot-b`. They are routing labels, not browser-profile names or provider identities. A controlled pairing flow assigns each installed extension instance to one available slot using an app-generated, single-use pairing capability. The capability is accepted only by the Native Messaging host, never sent to Claude, persisted in a browser profile, logged, or returned via IPC.

The host rejects a request when the extension ID is not allowlisted, the pairing capability is invalid or expired, the requested slot is not one of the two fixed slots, or the slot is already bound to a different extension instance. Re-pairing requires explicit local operator action that first invalidates the existing binding and atomically removes that slot's snapshot. This prevents silent duplicate-slot overwrite. The implementation must document how its extension-instance identifier is generated without reading a provider identity or browser path.

### Sanitized envelope schema

Only the following versioned object may leave the extension. Unknown fields, duplicate JSON keys, invalid types, invalid timestamps, out-of-range percentages, unknown window kinds, overlarge frames, and non-allowlisted errors are rejected.

| Field | Type / allowed values | Rules |
| --- | --- | --- |
| `version` | exact string `"v1"` | Schema version; no implicit compatibility. |
| `slot` | `"slot-a"` or `"slot-b"` | App-owned routing label only; host also checks pairing binding. |
| `source` | `"completion_sse"` or `"usage_endpoint"` | `completion_sse` is the only preferred production observation. |
| `observedAt` | RFC 3339 UTC instant | Extension observation time, not a provider identity or server trace. |
| `windows` | 1–2 entries | Each entry has `kind` (`"five_hour"` or `"weekly"`), optional `usedPercent` (0–100), optional `resetAt` (RFC 3339). Missing fields remain missing. |
| `status` | `"available"`, `"unavailable"`, or `"malformed"` | `available` requires a valid non-empty `windows` array. |
| `errorCode` | optional fixed enum | `"unavailable"`, `"malformed_payload"`, `"stale"`, `"host_unavailable"`, `"not_paired"`, `"unsupported_extension"`, or `"expired"`; no free text. |

Maximum implementation values must be explicit and tested before release: native-message body at most 8 KiB, at most two windows, field strings at most 64 UTF-8 bytes except RFC 3339 timestamps, and no nested objects beyond the window objects. Native Messaging framing uses the Chrome-required four-byte little-endian byte length followed by UTF-8 JSON; the host rejects a declared or actual length over the cap before parsing and never emits an unbounded response.

### Selection, retention, and freshness

For each slot, a valid `completion_sse` snapshot has precedence over `usage_endpoint`, irrespective of arrival order. A later endpoint snapshot must not replace it. A failed, malformed, unavailable, or stale SSE observation must not erase a valid completion-SSE snapshot. A valid endpoint snapshot may be retained only while no valid completion-SSE snapshot exists for that slot and TTL.

The provisional freshness TTL is **15 minutes** from `observedAt`. Read-only IPC returns `expired` with no usage windows after TTL; it does not turn stale data into zero or silently refresh. A newer valid completion-SSE observation atomically replaces the current snapshot. Clock skew handling, whether a valid endpoint may replace an expired SSE, and the UI distinction between `expired` and `unavailable` require Sol approval before implementation.

## Native Messaging distribution and lifecycle

The production extension must have a stable, release-controlled extension ID. The exact Chrome/Chromium support boundary is unresolved: the first implementation may support only managed Google Chrome stable if that is the only channel whose stable ID and native-host manifest installation can be verified. Chromium, Chrome Beta/Dev/Canary, other vendors, unpacked extensions, and developer-mode IDs are excluded unless separately approved.

| Lifecycle event | Owner and required action | Safe result |
| --- | --- | --- |
| Install | QuotaBar installer installs the signed native host and the manifest with exactly the production extension ID in `allowed_origins`; extension distribution is separately release-controlled. | No host accepts an unpacked or unknown ID. |
| First pairing | User explicitly pairs each profile to one app-owned slot. Host issues/consumes a one-use local capability. | No provider identity, profile path, or credentials are read. |
| App upgrade | QuotaBar installer updates the host and manifest atomically; schema compatibility is explicit. | Existing valid snapshots survive only if the store schema remains compatible; otherwise delete current snapshots and show unavailable. |
| Extension upgrade | Release pipeline preserves the stable extension ID and protocol compatibility, or requires a deliberate re-pair. | Unknown version/ID is rejected without fallback transport. |
| Chrome restart / extension reload | Extension reconnects only after a real observation; it does not heartbeat or poll. | Last snapshot is readable only until TTL, then expired. |
| QuotaBar restart | Host/store reopen with owner-only permissions and validate the current record before serving it. | Corrupt or invalid record is removed and returned as unavailable. |
| Missing host | Extension receives/records only `host_unavailable`; it must not retry in a tight loop or use localhost. | Existing snapshot ages out; recovery is user-visible safe state. |
| Uninstall / rollback | QuotaBar uninstaller removes host manifest/binary and securely removes current sanitized store; extension removal is included in product uninstall instructions. Rollback either preserves a protocol-compatible pair or invalidates pairing and deletes snapshots. | No orphan host accepts messages; no history remains. |

Production and validation must never share a namespace. The validation candidate uses a distinct bundle ID, native-host name, extension ID, config/store directory, and `allowed_origins` entry; it cannot read, write, or register over installed QuotaBar state.

## Failure and recovery semantics

| Condition | Store/IPC state | Recovery behavior |
| --- | --- | --- |
| Sleep/wake | Current snapshot remains until TTL; no synthetic refresh. | Next real observation may update it. |
| Network loss | No extension request is originated; no new snapshot is created. | Existing snapshot expires normally. |
| Chrome/profile restart | Current snapshot remains until TTL. | Extension emits only after an actual qualifying observation. |
| Host unavailable or malformed native frame | Preserve last valid snapshot until TTL; fixed safe error may be surfaced without raw detail. | Reconnect only through normal extension lifecycle, with bounded backoff and no polling. |
| Invalid schema, oversize frame, unknown slot/ID | Drop input; do not write store. | Require released compatible extension or explicit re-pairing. |
| Store corruption or permission failure | Delete/reject invalid record; IPC returns unavailable. | Do not reconstruct from logs or browser data. |
| Duplicate pairing / re-pair | Reject duplicate; explicit re-pair atomically clears old binding and snapshot. | Operator repeats pairing for the intended slot. |

## Component and file impact map

The map is a future implementation boundary, not permission to edit these files in this ADR PR.

| Layer | Expected location / component | Future responsibility | Must not do |
| --- | --- | --- | --- |
| Extension | New separately packaged production extension | Same-origin observation, bounded parsing, sanitization, Native Messaging client, explicit pairing UI. | Read/export credentials; provider polling; raw payload storage; use P4B probe source. |
| Native host | New QuotaBar-owned signed host executable and native-host manifest | Extension-ID allowlist, native framing, schema/pairing validation, atomic store write. | Listen on localhost; accept arbitrary extension IDs; expose provider data. |
| Rust store | New production service under `src-tauri/src/services/` | Validate/read max-two sanitized records and permissions. | Reuse `claude_bridge.rs` or store raw input/history. |
| Tauri commands | `src-tauri/src/commands.rs` and `src-tauri/src/lib.rs` in a later PR | Register one read-only sanitized snapshot query. | Change or reroute `get_quota`. |
| Domain DTO | `src-tauri/src/domain/` in a later PR | Explicit public sanitized DTO and fixed error enum. | Include identity, path, credential, raw JSON, or arbitrary error text. |
| Frontend / tray | Deferred Stream D files, including `src/App.tsx`, `src/components/ClaudePanel.tsx`, summary/tray services | Consume read-only DTO after transport acceptance. | Start in Stream C or alter existing Claude Code semantics. |
| Packaging | Tauri bundle/install/uninstall configuration in a separately approved PR | Install/remove host and isolate validation namespace. | Overwrite installed QuotaBar during validation. |

## Acceptance matrix for the implementation PR

| Acceptance area | Evidence required | Mandatory failure |
| --- | --- | --- |
| Native Messaging only | Static inspection plus focused integration test proves no HTTP listener, port, WebSocket, or localhost fallback. | Any local listener or alternate transport. |
| Stable release identity | Installed manifest's `allowed_origins` contains only the approved stable extension ID; validation ID is separate. | Unpacked/developer/unknown ID accepted. |
| Sanitization | Fixtures and logs prove prohibited fields and sentinels cannot cross any boundary or persist. | Raw payload, identity, cookie/token/session data, URL route, conversation data, or free-text provider error. |
| Framing/schema | Oversize, malformed, duplicate-key, unknown-field, unknown-slot, and invalid timestamp/window tests fail closed. | Input reaches store/IPC despite validation failure. |
| Slot isolation | Two slots cannot overwrite, read, or infer each other; duplicate pairing and explicit re-pair are tested. | Cross-slot leakage or implicit reassignment. |
| Precedence/freshness | Arrival-order tests prove valid SSE outranks endpoint and failed/later SSE cannot erase it; 15-minute expiry returns no usage windows. | Endpoint overwrites valid SSE, stale data becomes zero, or failed SSE erases valid state. |
| Atomic retention | Interruption/corruption tests prove max-two current `0600` records and no history/raw payload. | World-readable, partial, historical, or raw storage. |
| Lifecycle | Install, update, host missing, Chrome/app restart, rollback, uninstall, and validation namespace isolation have testable procedures. | Production/validation state or identity can collide. |
| Regression | Existing `get_quota` behavior and all repository release checks remain passing. | Any reroute or behavior change to Claude Code path. |
| Human gate | Sol High reviews exact diff, architecture/security tests, and macOS 12 runtime plan before Stream C merges. | Implementation proceeds without approval. |

## Explicit unresolved decisions — block implementation until resolved

1. **Stable extension distribution and ID:** choose the signed distribution channel, obtain the immutable production ID, and state the supported Chrome/Chromium versions. The native host cannot be implemented safely without it.
2. **macOS install ownership:** specify signed/notarized host packaging, host manifest location, install privilege model, upgrade rollback behavior, and verified uninstall deletion. The current repository has no approved production host installer.
3. **Pairing UX and capability durability:** define the app-generated one-use pairing capability format, expiry, local storage location, and recovery UX without creating a credential-like secret or leaking profile data.
4. **Protocol details:** approve the exact window semantics (`weekly` reset representation), clock-skew policy, endpoint replacement after SSE TTL, and whether safe error metadata coexists with a still-readable valid snapshot.
5. **Validation product boundary:** approve the separate bundle ID, host name, config path, extension ID, and installer process; verify it cannot collide with installed QuotaBar.
6. **Runtime evidence:** define the macOS 12 Chrome and QuotaBar restart/sleep-wake test matrix and the operator consent required before live authenticated observations or message sending.

## Alternatives rejected now

| Alternative | Decision | Reason |
| --- | --- | --- |
| Localhost listener / HTTP or WebSocket bridge | Rejected | Expands local attack surface and lacks Native Messaging extension-origin allowlisting. |
| Promote P4B temporary probe | Rejected | It is an acquisition-only validation artifact with a separate namespace and no production lifecycle. |
| Promote P3 `claude_bridge.rs` | Rejected | It is synthetic test code and endpoint-first selection conflicts with the production SSE rule. |
| Read Chrome profile storage or export cookies/tokens | Rejected | Violates credential ownership and privacy boundary. |
| Reuse Claude Code `get_quota` | Rejected | It is a separate OAuth compatibility path and must remain unchanged. |

## Required review outcome

Sol High must explicitly approve or block this ADR's Native Messaging design, stable-ID/distribution plan, schema/privacy boundary, two-slot pairing model, retention/freshness policy, and validation isolation. Until then, the only permitted next work is refinement of this document and review evidence; Stream C implementation remains blocked.
