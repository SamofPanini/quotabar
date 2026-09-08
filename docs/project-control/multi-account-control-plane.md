# Multi-account control plane

## Phase state

| Phase | State | Evidence |
| --- | --- | --- |
| P0 | Cloud-complete; local smoke pending | Cloud checks passed; desktop behavior remains unverified locally. |
| P1 | Merged | `16997c93...` |
| P2 | Merged | [`daa92492288405f5224cfedeb92c1706862ee3ee`](https://github.com/SamofPanini/quotabar/commit/daa92492288405f5224cfedeb92c1706862ee3ee), [PR #2](https://github.com/SamofPanini/quotabar/pull/2), [PR CI](https://github.com/SamofPanini/quotabar/actions/runs/34231684473), [main CI](https://github.com/SamofPanini/quotabar/actions/runs/34233334042) |
| P3 | Not started | Dispatch only from the P3 brief below. |

Read this file first, then [P3-claude-bridge-spike.md](P3-claude-bridge-spike.md), then the files named in that brief. The exact commit named by a task is the task base: inspect it before writing, and never silently substitute a newer head.

## Control rules

- One implementation branch has one writer. Auditors are read-only and do not amend that writer's branch.
- Terra implements scoped code. Luna performs bounded read-only audits and fixture/test-matrix checks. Sol High accepts architecture, isolation, credential safety, and final scope.
- Every PR declares its base SHA, file allowlist, verification result, and whether local-only gates remain pending.
- Stop rather than broaden scope when the base is no longer exact, a required behavior needs a real credential/session, a new public API is needed, generated changes touch files outside the allowlist, or a security boundary is unclear.

## Canonical cloud CI gate

Run from the repository root, in this order:

```bash
npm ci
npm run release:check
npm test
npm run build
cargo fmt --manifest-path src-tauri/Cargo.toml --check
cargo check --manifest-path src-tauri/Cargo.toml
cargo test --manifest-path src-tauri/Cargo.toml
```

Passing this gate means only that the committed source passed CI's macOS runner. It does not establish desktop login, IPC, tray, profile, or Keychain behavior on the target machine.

## Credential boundary

Provider-derived credentials and raw identifiers are read-only inputs at the provider edge. Never commit, return, render, cache in public objects, or log tokens, cookies, authorization headers, session keys, provider account/organization identifiers, email addresses, raw provider payloads, or local credential locations. App-generated opaque `instanceId` and account-route keys may isolate state, but must not be derived from an organization, email, token, cookie, or other provider credential/identity. Error text must be safe to expose. A request to capture or replay any forbidden value is a stop condition.

## Local macOS 12 Intel gates

Cloud Codex cannot close these gates. On the target macOS 12 Intel machine, verify:

1. app startup and stable tray/window lifecycle;
2. Tauri command/IPC invocation and failure behavior;
3. default Claude account plus two custom Codex profiles;
4. configured path aliases resolve to the intended profile without cross-profile reuse;
5. Keychain/environment credential discovery, expiry, rotation, and redacted diagnostics;
6. no credential, account, or raw-response value is displayed or persisted in visible logs.

## Current task matrix

| Work item | Cloud Codex may do now | Must remain local / separately approved |
| --- | --- | --- |
| Documentation and contract design | Write and review docs-only PRs; inspect exact repository base and CI definitions. | None. |
| P3 parser/normalizer spike | Implement synthetic contract, redaction validator, fixtures, and isolation tests after explicit dispatch. | Real provider login, endpoint calls, listener, IPC, desktop UI, or transport. |
| P1/P2 regression review | Read-only source/test audit and canonical CI. | Actual multi-account UI and profile behavior. |
| Credential handling | Review type/log/error boundaries without secrets. | Reading, exporting, copying, or validating live credentials/sessions. |
| Release confidence | Verify CI and docs. | macOS 12 Intel app bundle, tray, Keychain, installation, and live quota smoke. |

P3 is intentionally a narrow contract spike, not permission to start an integration or a multi-account product surface.
