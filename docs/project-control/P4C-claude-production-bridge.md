# P4C Claude Desktop companion ADR

## Status, decision, and scope

- **Status:** `S0-B / ISOLATED_COMPANION_POC_REQUIRES_OWNER_ACCEPTANCE`.
- **Supersession:** for the current Desktop-only track, this ADR supersedes the prior Chrome Native Messaging implementation decision. Chrome/CWS is not an active production route.
- **Authorization:** C2 authorizes documentation only. It does not authorize a companion build, installation, launch, login, Claude message, runtime observation, source implementation, store, IPC, UI, test, or distribution change.
- **Official app:** the installed official `/Applications/Claude.app` remains untouched.

The companion described here is a separately authorized, unsupported, temporary validation client only. It is not QuotaBar production architecture and does not establish a supported production source.

The following paths are rejected for this track: Chrome/CWS, Claude Code credential substitution, direct injection into the official app, CDP/DevTools, MITM or proxying, Accessibility/OCR, and a generic wrapper or WebView.

## Future trust-boundary topology

If a later owner decision authorizes a PoC, it must use a distinct validation bundle identifier and display name, a distinct install and receipt root, exactly two initial slot-specific user-data roots (one owner-designated Paid slot and one owner-designated Free slot), a distinct companion observer-state root, and a distinct QuotaBar validation-state namespace.

Before any future launch, canonical-path, owner, type, and collision checks must establish that every such root is unique and owned as expected. No official-app path, official user data, unrelated Keychain item, or production QuotaBar state may be mutated.

The future conceptual data flow is strictly:

```text
companion page world -> clean-room bounded parser -> sanitized observation envelope
  -> future QuotaBar validation ingress -> W1 current-only store
```

No code or executable configuration is defined by C2.

## Sanitize-before-crossing envelope

The future validation ingress accepts exactly one decoded discriminated union named `ObservationEnvelopeV1`. Transport framing and authentication may be selected in C3-B, but before any state mutation the ingress rejects duplicate keys, unknown fields, missing required fields, extra variant fields, non-finite numbers, and out-of-range values. Common required fields are `schemaVersion = v1`; `status = available | unavailable`; `accountSlotId`, a canonical lowercase UUIDv4 issued by QuotaBar during foreground binding; `bindingEpoch` and `sequence`, canonical unsigned decimal strings in the inclusive range 1 through 18446744073709551615; and `observedAt`, an RFC 3339 UTC timestamp ending in `Z`.

For `status = available`, `source` is exactly `completion_sse` and `windows` is a non-empty array of one or two exact window objects in deterministic `five_hour`, then `weekly` order. Each window contains `kind = five_hour | weekly`, finite `usedPercent` in the inclusive range 0 through 100, and optional `resetAt` as an RFC 3339 UTC timestamp ending in `Z`. Duplicate kinds are rejected and `errorCode` is forbidden.

For `status = unavailable`, `errorCode` is exactly `unavailable | malformed_payload | unsupported_observation`; `source` and `windows` are forbidden. Missing values never default to zero. A rejected object, wrong epoch, replay, or skipped sequence consumes no sequence and mutates no store or safe metadata.

It must reject raw response, SSE, or endpoint bodies; prompt or response text and arbitrary strings; URL, route, and request headers; tokens, cookies, sessions, authorization, or credential material; provider account, organization, email, or plan identity; filesystem paths; and raw debugging payloads. Raw acquisition material remains in the source boundary and is never persisted or logged.

## Binding, account change, and epoch ceremony

W1-R1 governs the binding lifecycle. Reset and expiry never unbind a slot; Paid and Free are metadata, never identity. Initial binding is an explicit foreground action. Continuity-proven observations remain in the current epoch.

The current `BindingEpoch` is required and is the sole v1 ingestion correlation field together with `AccountSlotId`. It uses the canonical unsigned decimal representation defined by `ObservationEnvelopeV1`. No opaque correlation field may substitute for `BindingEpoch`. A later transport may add a request-local nonce solely for acknowledgement correlation or identical retry handling, but that nonce never determines identity, epoch, storage, DTO, or display state.

On logout, a proven different account, or continuity uncertainty, all old numeric windows are hidden before acknowledgement. Different-account and re-pair operations advance the epoch; uncertainty sets `bindingState = unverified`. Delayed or replayed observations from a prior epoch are rejected. Alias, tab position, plan class, or similar percentages never establish identity. If a candidate source cannot provide trustworthy continuity, it must require explicit foreground confirmation and rebind.

## W1-R1 storage and DTO dependency

This ADR references W1-R1 and neither duplicates nor weakens it. W1-R1 requires one current record per slot, epoch, and window; independent five-hour and weekly projection; partial updates that preserve the other valid window; durable expiry and non-resurrection under clock rollback; and a current-only atomic aggregate with no history, analytics, or raw-event cache.

Offline reads are local only and create no provider contact. The read-only DTO exposes only a safe alias, slot binding state, independent window projections, and fixed safe error codes. C2 does not implement the store, IPC, or UI.

## Signing, update, and compatibility policy

Any future validation companion is independently or ad-hoc signed and non-notarized unless a later owner decision changes that policy. No automatic update path may point to or overwrite official Claude. Updater divergence, Keychain prompts, Cowork loss, and provider drift are accepted PoC risks, not production guarantees.

macOS 12 Intel feasibility is best-effort and must be proven locally. This ADR makes no public-distribution or maintained-production claim.

## License and clean-room boundary

`lugia19` projects are mechanism and reference evidence only. QuotaBar must not copy GPL source, bundled extension code, or generated artifacts from them. Any future observer or parser must be independently authored from this closed contract and synthetic fixtures. Dependency and license provenance must be recorded before any future code is accepted.

## Future synthetic gates

Before any live authorization, future work must define and pass synthetic gates for:

- namespace collision and canonical-path rejection;
- official-app before/after immutability;
- schema and forbidden-field rejection;
- Paid/Free slot isolation;
- late, replayed, and wrong-epoch rejection;
- partial-window preservation;
- reset without unbinding;
- expiry and clock-rollback non-resurrection;
- crash-safe old-or-new aggregate semantics;
- offline read with Claude closed;
- bounded cleanup and exact ownership;
- logs, DTO, and debug redaction; and
- no polling, heartbeat, watcher, or background retry.

## Future live gate and separate authorization

A future live gate requires a new owner decision and cannot be inferred from this ADR. It is limited to exactly one Paid and one Free account, with at most one ordinary message per account. No retry, regenerate, or additional message is allowed without new authorization. Each companion instance uses normal independent login only; credential or session export and replay are prohibited.

The future live gate also requires exact cleanup and uninstall proof and an immediate stop upon any S0 stop condition.

## Stop conditions and non-claims

Future work stops immediately on official path or bundle mutation; namespace collision; unapproved Gatekeeper bypass; credential or session export; raw or forbidden data crossing; unexplained update or network behavior; cross-slot contamination; inability to fail closed on identity uncertainty; macOS 12 launch or signing instability; unaccepted provider-policy uncertainty; or cleanup ownership mismatch.

This ADR makes no claim of Anthropic support, production admissibility, or future Claude-version patchability. It makes no claim to observe messages sent in the installed official app, prove account identity without explicit binding or rebinding, or authorize building or running a PoC.

## Mandatory next step

This documentation-only decision requires one ADR PR, exact-head CI, and independent Sol High acceptance before the Work control plane may make any separate companion decision. After those records exist, the C2 executor must stop; C3, L1, installation, login, and runtime validation remain out of scope.
