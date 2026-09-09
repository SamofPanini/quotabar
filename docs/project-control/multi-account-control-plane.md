# Multi-account control plane

## Phase state

| Phase | State | Evidence |
| --- | --- | --- |
| P0 | Cloud-complete; local smoke pending | Cloud checks passed; desktop behavior remains unverified locally. |
| P1 | Merged | `16997c93...` |
| P2 | Merged | [`daa92492288405f5224cfedeb92c1706862ee3ee`](https://github.com/SamofPanini/quotabar/commit/daa92492288405f5224cfedeb92c1706862ee3ee), [PR #2](https://github.com/SamofPanini/quotabar/pull/2), [PR CI](https://github.com/SamofPanini/quotabar/actions/runs/34231684473), [main CI](https://github.com/SamofPanini/quotabar/actions/runs/34233334042) |
| P3 | Accepted / Merged | [PR #4](https://github.com/SamofPanini/quotabar/pull/4), accepted head `caf6154e778b5483e13465c675eda4311880e260`, [CI run 34306236389](https://github.com/SamofPanini/quotabar/actions/runs/34306236389), merge `9827579eed1bc465537d42c52ff916feaffafb4f`. |

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


## P3 final acceptance — 2026-09-09

- Sol High reviewed exact head `c805ddbc3434b11ad3e6b197b410ee0bfdfc400d` and found one concrete Medium blocker: the synthetic bridge module was declared unconditionally, contradicting the test-only production boundary and generating extensive dead-code warnings.
- The only remediation was `#[cfg(test)] mod claude_bridge;` in `src-tauri/src/services/mod.rs`; no tests, dependencies, runtime paths, or scope were added.
- Accepted head `caf6154e778b5483e13465c675eda4311880e260` passed canonical macOS CI [run 34306236389](https://github.com/SamofPanini/quotabar/actions/runs/34306236389). Production `cargo check` no longer emitted `claude_bridge.rs` warnings, while all existing bridge contract tests still ran.
- PR #4 was squash-merged as `9827579eed1bc465537d42c52ff916feaffafb4f`.
- `P3 = Accepted / Merged`. P4 remains unstarted and requires a new explicit dispatch.

## Local validation decision points

| Node | Local macOS 12.7.5 Intel action | Decision value |
| --- | --- | --- |
| Now: P2 production backend | Run the app against the default Codex account and two custom `CODEX_HOME` profiles; exercise alias rejection, credential rotation, Keychain/environment discovery, startup, tray/window lifecycle, and redacted failures. | P2 changed real credential-route and cache behavior. Cloud CI cannot prove Monterey filesystem, Keychain, or app lifecycle behavior. |
| Not now: P3 contract spike | Do not run Claude paid/free performance tests solely for P3. | P3 is compiled only under `cfg(test)` and has no production transport, IPC, UI, persistence, or runtime call path; local benchmarking would measure unrelated legacy behavior. |
| Mandatory before merging the first real Claude bridge | As soon as a future phase introduces a real transport/process/profile seam or Tauri IPC, validate one paid and one free account before adding multi-account UI. Compare feature branch against the same-machine baseline for startup, steady CPU/RSS, refresh latency, failure isolation, logout/rotation, sleep/wake, and network loss. | This is the first point where macOS 12 compatibility, live login, resource cost, and long-running stability become observable. A failure here blocks UI expansion. |
| Mandatory before release | Validate the packaged app, Gatekeeper/Keychain interaction, tray/window lifecycle, all supported profiles, recovery after sleep/network change, redacted diagnostics, and an extended idle/refresh soak. | Release confidence requires the target OS, packaging, and real lifecycle; cloud tests cannot substitute. |

Use baseline-relative evidence on the same Mac. Start with one baseline run and one feature run; repeat only when variance or a failure needs diagnosis. Do not create synthetic performance tests for code that is not reachable in production.

If the current Codex.app build cannot run on macOS 12, use Codex CLI or Terminal to drive the same checkout and commands. The required evidence comes from the Monterey runtime and packaged application, not from the controller UI.
