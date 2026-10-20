# R4-STATUS-HISTORY 返工 2（Sam 10-20 实测复现后追加）

返工 1 已核：W1–W5 落实，控制面第二轮变异 23 项中除 q4（结构保证，预期存活）与两项锚点失效外全部被杀。本轮只做下面两条，其余不动。每条写落实位置（文件:行）与测试名，回填 report.md 新增「§返工 2」节（不删前两节）。

## W6 外部续期后不必等 60 分钟（Sam 10-20 实测）
实测时间线（`~/Library/Logs/quotabar/claude.log`，控制面已核）：02:14 令牌到期 → 认证错误 ⇒ 前端把下一次自动拉取排在 `AUTH_REFRESH_INTERVAL_MS`（60 min）后；02:16 Sam 手动续期后的两次拉取成功，但此后 40 分钟没有任何轮询。R4 §1 的重排已覆盖「续期后经 QuotaBar 刷新／ping 后刷新」这一路径；**未覆盖的是：在 QuotaBar 之外续期（终端里的 Claude Code、桌面端），之后没有任何 QuotaBar 侧拉取** ⇒ 仍要等满 60 分钟。
行为契约：
1. 当最近一次 Claude 结果是**认证错误**（`isClaudeAuthError`；signed-out 判定照旧）时，每 60 秒做一次**本地**检查：读钥匙串里的 Claude Code 凭证的到期时刻（`expires_at`），**不发任何网络请求**、不把令牌值传到前端或日志。
2. 到期时刻与「失败时的那张凭证」不同（含从无到有）⇒ 立即触发一次正常的 Claude 拉取（走 `fetchClaudeQuota`，于是 §1 的重排接管后续节奏）。相同 ⇒ 什么也不做，60 分钟的 API 退避保持不变。
3. 离开认证错误状态（任何成功拉取、卸载、`claudeAutoRenewSynced` 关闭）即停止这个本地检查；任一时刻最多一个检查定时器。
4. 后端已有按 `expires_at_ms` 的认证失败闸（`claude.rs` `quota_request_gate`／`login_is_blocked` 一带），复用它的凭证读取函数；新接口只返回到期时刻（或「是否与失败凭证不同」的布尔），**不得返回令牌**。
具名测试：
- 前端（vitest，经 App 生产路径，fake timers）：`auth_error_polls_local_credential_expiry_every_minute`、`changed_credential_triggers_immediate_fetch`、`unchanged_credential_waits_for_auth_backoff`（60 秒检查 ×N 均不触发 `getQuota`）、`credential_check_stops_after_success_and_on_unmount`。
- 后端（Rust）：新接口在凭证相同／不同／缺失三种情形下的返回；**断言返回值里不含令牌字段**（经序列化结果断言）。

## W7 测试不得写真实日志
现状：`claude.rs` 与 `codex.rs` 的 `log_msg` 无条件写 `~/Library/Logs/quotabar/{claude,codex}.log`；`cargo test` 期间测试会把「credentials missing」「auto-renew sent」等假事件写进 Sam 正在使用的真实日志（控制面诊断 W6 时被这些行干扰）。
要求：`#[cfg(test)]` 下 `log_msg` 不写 `~/Library/Logs`（可写入测试可读的内存缓冲或临时目录；既有依赖日志内容的测试改用该途径）。生产行为逐字不变。
测试：`test_logging_does_not_touch_user_log_dir`（`claude.rs` 与 `codex.rs` 各一条，断言测试期间解析出的日志路径不在 `~/Library/Logs` 之下，或写入走缓冲）。

## 白名单（在 spec §7 基础上追加）
`src-tauri/src/commands.rs`（仅新增 W6 的只读命令）、`src-tauri/src/services/codex.rs`（仅 W7 的 `log_msg`）、`src/services/backend.ts`、`src/services/app_state.ts`。注册新命令所需的 `src-tauri/src/lib.rs` 改动允许。改调用签名前先读 `scripts/check_latest_request_wiring.mjs`（只做让检查器继续通过的最小调整，不得放宽判据）。不新增依赖、不改 capability 文件；若新命令必须改 capability 才能调用，停下交 `BLOCKED_ALLOWLIST` 并写明需要改哪一行。

## 不变
spec.md §4／§5／§5b／§8 照旧。本轮不跑变异（控制面交付后复跑）。末行唯一 verdict：`READY_FOR_MUTATION` 或 `BLOCKED_<大写原因>`。
