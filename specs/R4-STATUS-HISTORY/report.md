# R4-STATUS-HISTORY report

## §0 复核结论

- 开工自检与任务单一致：HEAD `dfd130350f2457dd7bbaa07d8917400da9a63237`，分支 `codex/r4-status-history`，初始 porcelain 唯一条目为 `?? specs/R4-STATUS-HISTORY/`，Node `v22.23.2`，`/` 可用 4.4 GiB。
- 基线全量 Rust 测试：460 passed、2 ignored，exit 0。
- 源码复核：`fetchClaudeQuota` 实际在 `src/App.tsx:409`，原轮询 effect 位于其后；`build_status` 的实际起点为 `service_status.rs:1414`（与任务单“约 1268”有出入，代码在 fork 中较长）；`fetch_quota` 成功解析路径实际在 `claude.rs:1507` 附近。实现按实际锚点完成。

## §1 Claude 轮询重排

- `src/App.tsx:200-201`：唯一 timer 与调度函数 ref。
- `src/App.tsx:434,441`：仅 current generation 的成功/错误完成路径重排。
- `src/App.tsx:449-473`：同步状态变化和卸载时清除 timer；timer 触发仅走同一条 `fetchClaudeQuota` 路径。
- `tests/claude_auth_recovery.test.tsx:241-305`：`auth_error_then_ping_refresh_reschedules_to_one_minute`、`manual_refresh_success_cancels_pending_backoff_timer`、`out_of_loop_error_reschedules_with_backoff`、`stale_generation_result_does_not_reschedule`、`unmount_clears_claude_poll_timer`。

## §2 已解决事故保留 24 小时

- `src-tauri/src/services/service_status.rs:101-117`：`ResolvedServiceEvent` 与 serde `recentlyResolved` 数据契约。
- `src-tauri/src/services/service_status.rs:478-596`：仅成功且非 unknown 的轮询结果进行比对；按 feed、一次 incidents 查询、观察时刻的优先序解析解决时间。
- `src-tauri/src/services/service_status.rs:1631-1679`：过期清理、重开移除、按解决时间倒序与 alert 清除。
- `src-tauri/src/services/service_status.rs:1681-1750`：0600 临时文件 rename 持久化与损坏文件降级；启动回读在 `:222-251`。
- `src/services/service_status.ts:9-10`、`src/components/ServiceStatusNotice.tsx:7-35`、`src/styles/content.css:92-95`：前端中性已解决提示、精确/估算时间文案、各两条上限和合并 more 计数。
- `service_status.rs:3681-3829`：`incident_disappearing_on_success_moves_to_recently_resolved`、`failed_fetch_does_not_resolve_incidents`、`resolved_at_prefers_feed_then_incidents_lookup_then_observed`、`resolved_entries_expire_after_24h`、`reopened_incident_leaves_recently_resolved`、`recently_resolved_does_not_change_alert_level_or_notifications`、`resolved_store_round_trips_and_ignores_corrupt_file`。
- `tests/service_status_ui.test.tsx:29-68`：`resolved_only_renders_neutral_notice_with_resolved_time`、`estimated_resolution_says_resolved_by`、`active_and_resolved_render_active_first_and_cap_two_each`、`no_resolved_and_operational_still_renders_nothing`。

## §3 Claude 额度 JSONL 快照

- `src-tauri/src/services/quota_history.rs:52-289`：尾行加载、差异/30 分钟心跳追加、120 天每日最多一次裁剪、0600 与临时文件 rename；不在请求路径全文件扫描。
- `src-tauri/src/services/claude.rs:1516-1518`：真实 200 且成功解析后的生产调用；缓存、stale 回退、错误与 disconnected 路径在此前返回，不能调用记录器。
- `src-tauri/src/lib.rs:90`：进程启动初始化历史尾行与裁剪状态；`services/mod.rs:17` 注册模块。
- `quota_history.rs:315-389`：`writes_line_with_v1_schema_on_fresh_success`、`identical_values_within_30_minutes_are_not_rewritten`、`heartbeat_written_after_30_minutes`、`cache_hit_stale_and_error_paths_do_not_record`、`prune_drops_lines_older_than_120_days_and_unparseable_lines`、`history_file_is_0600`。

## 测试 → 测试名

| 测试 | 真实名称 |
| --- | --- |
| Claude 重排 | `auth_error_then_ping_refresh_reschedules_to_one_minute`; `manual_refresh_success_cancels_pending_backoff_timer`; `out_of_loop_error_reschedules_with_backoff`; `stale_generation_result_does_not_reschedule`; `unmount_clears_claude_poll_timer` |
| 已解决后端 | `incident_disappearing_on_success_moves_to_recently_resolved`; `failed_fetch_does_not_resolve_incidents`; `resolved_at_prefers_feed_then_incidents_lookup_then_observed`; `resolved_entries_expire_after_24h`; `reopened_incident_leaves_recently_resolved`; `recently_resolved_does_not_change_alert_level_or_notifications`; `resolved_store_round_trips_and_ignores_corrupt_file` |
| 快照后端 | `writes_line_with_v1_schema_on_fresh_success`; `identical_values_within_30_minutes_are_not_rewritten`; `heartbeat_written_after_30_minutes`; `cache_hit_stale_and_error_paths_do_not_record`; `prune_drops_lines_older_than_120_days_and_unparseable_lines`; `history_file_is_0600` |
| 已解决 UI | `resolved_only_renders_neutral_notice_with_resolved_time`; `estimated_resolution_says_resolved_by`; `active_and_resolved_render_active_first_and_cap_two_each`; `no_resolved_and_operational_still_renders_nothing` |

## 验证结果（逐项）

- `cargo fmt --check --manifest-path src-tauri/Cargo.toml` → exit 0。
- `cargo check --offline --locked --tests --manifest-path src-tauri/Cargo.toml` → exit 0（既有 dead-code warnings）。
- `cargo test --offline --locked --manifest-path src-tauri/Cargo.toml 2>&1 | tail -60` → exit 0；473 passed、2 ignored。
- `npx tsc --noEmit` → exit 0。
- `npx vitest run` → exit 0；61 files、707 tests passed。
- `TZ=UTC npx vitest run` → exit 0；61 files、707 tests passed。
- `node --test scripts/*.test.mjs` → exit 0；102 pass、0 fail。
- `node scripts/check_release_version.mjs` → exit 0；release manifests agree on 0.4.14。
- `node scripts/check_latest_request_wiring.mjs` → exit 0。
- `git diff --check` → exit 0。

## 交付物清单

- 轮询重排、已解决事故回溯/持久化/UI、Claude `quota-history.jsonl` 记录模块及其测试。
- 白名单核对：所有修改均位于 §7 白名单；未改依赖、版本、CHANGELOG、Cargo.toml、Cargo.lock 或 package.json；未运行变异测试。

## 规格偏差

- 前端轮询的五项具名测试放在既有 `tests/claude_auth_recovery.test.tsx`（可复用既有 App/后端夹具），而不是任务单建议的新 `tests/claude_poll_reschedule.test.tsx`；测试名、fake-timer 行为和生产 App 路径均已覆盖。其余无偏差。

READY_FOR_MUTATION

## §返工 1

### 交付物清单

- W1：`src-tauri/src/services/service_status.rs:172,512-645,1707-1723` 为每个 provider 保留最后一次成功快照作为 incident 比对基线；失败／unknown 与 30 分钟 stale 替换后均只延续并裁剪已有 `recently_resolved`。测试均经 `poll_once_with_fetchers_and_lookup_at` 驱动：`failure_keeps_recently_resolved_until_expiry`（`:3779`）、`incident_resolved_during_outage_is_detected_after_recovery`（`:3807`）、`stale_failure_replacement_keeps_recently_resolved`（`:3838`）；既有 `failed_fetch_does_not_resolve_incidents` 保留（`:3765`）。`persist_recently_resolved` 没有路径注入缝，且直接路径会访问实际 state location（本任务禁止读取该目录），故未新增 `failed_poll_does_not_empty_resolved_store`；生产轮询仍在 `service_status.rs:350-353` 每次 poll 后持久化，失败快照现在保有同一 resolved 列表。
- W2：`src/components/ServiceStatusNotice.tsx:7-12` 在渲染前过滤早于 24 小时、或无法解析 `resolvedAt` 的 resolved 项；空结果仍走 `return null`。`resolved_older_than_24h_is_not_rendered`（`tests/service_status_ui.test.tsx:69`）覆盖仅有过期项及非法时间项。
- W3：`src-tauri/src/services/quota_history.rs:52-74` 在启动后以后台阻塞任务运行首次／每日裁剪；`record_success_at`（`:103-106`）不再触发全文件裁剪；`prune_if_due_at_with_before_rename`（`:270-290`）用与追加相同的锁涵盖 read→temporary→rename。测试：`record_success_does_not_prune_inline`（`:421`）与在 rename 前注入追加的 `append_during_prune_is_not_lost`（`:437`）。
- W4：`src-tauri/src/services/quota_history.rs:122,136-164` 将可解析的 `resets_at` 按分钟比较、解析失败回退逐字比较，仍写入原始字符串。测试：`subminute_resets_at_jitter_is_not_a_change`（`:476`）与 `resets_at_minute_change_is_a_change`（`:494`）。
- W5：删除恒真 `should_record_successful_response` 及恒真测试；真实 200、完成解析且生成连接结果后唯一生产记录点是 `src-tauri/src/services/claude.rs:1516`。缓存／stale／错误分别在 `claude.rs:1316-1318,1431-1461,1464-1475,1494-1505` 提前返回，不能到达记录点；未为此新造恒真测试。
- 变异补测：a4 的 `thrown_error_reschedules_with_its_interval` 在 `tests/claude_auth_recovery.test.tsx:280` 验证抛出的 429 按 5 分钟且仅一次重排；a5 强化 `stale_generation_result_does_not_reschedule`（`:296`）为最新结果先完成、过期结果后返回，精确断言 60 秒调用时刻；r6 的 `recently_resolved_is_sorted_newest_first` 位于 `service_status.rs:3940`；q4 由 W5 的唯一真实 200 生产调用点覆盖。

### 验证结果（逐项）

- `cargo fmt --check --manifest-path src-tauri/Cargo.toml` → exit 0。
- `cargo check --offline --locked --tests --manifest-path src-tauri/Cargo.toml` → exit 0（既有 dead-code warnings）。
- `cargo test --offline --locked --manifest-path src-tauri/Cargo.toml 2>&1 | tail -60` → exit 0；480 passed、2 ignored。
- `npx tsc --noEmit` → exit 0。
- `npx vitest run` → exit 0；61 files、709 tests passed。
- `TZ=UTC npx vitest run` → exit 0；61 files、709 tests passed。
- `node --test scripts/*.test.mjs` → exit 0；102 pass、0 fail。
- `node scripts/check_release_version.mjs` → exit 0；release manifests agree on 0.4.14。
- `node scripts/check_latest_request_wiring.mjs` → exit 0。
- `git diff --check` → exit 0；本轮未运行变异。
- 白名单核对：修改／新增仅为 `src/App.tsx`、`src/components/ServiceStatusNotice.tsx`、`src/services/service_status.ts`、`src/styles/content.css`、`src-tauri/src/lib.rs`、`src-tauri/src/services/{claude.rs,mod.rs,service_status.rs,quota_history.rs}`、`tests/{claude_auth_recovery.test.tsx,service_status_ui.test.tsx}` 与 `specs/R4-STATUS-HISTORY/report.md`，均在 §7；未改依赖、版本、CHANGELOG、Cargo.toml、Cargo.lock 或 package.json，未提交／推送。

### 规格偏差

- 开工实测 HEAD 与分支符合任务单：`dfd130350f2457dd7bbaa07d8917400da9a63237`／`codex/r4-status-history`，Node `v22.23.2`；但 porcelain 已含 r0 的 10 个修改文件、1 个新 Rust 模块及 `?? specs/R4-STATUS-HISTORY/`，并非 §8 所述唯一 specs 条目；`df -h /` 可用 3.2 GiB（非 r0 报告的 4.4 GiB），仍高于 2.5 GiB 门槛。本轮按任务要求以工作区现状返工。
- 无其他偏差。

READY_FOR_MUTATION

## §返工 2

### 交付物清单

- W6：`src-tauri/src/services/claude.rs:303-310,820-857,1036-1065` 新增只含 `changed` 布尔值的凭证变化投影；它复用 Claude Code 钥匙串读取器，只读取 `expires_at` 元数据，不传递或记录令牌。认证失败时记录失败凭证的到期值（包括缺失凭证的 `None`），同值／读取失败不触发拉取，不同值（包括无到有）才触发。
- W6：`src-tauri/src/commands.rs:20-23`、`src-tauri/src/lib.rs:50-52`、`src/services/backend.ts:77-79` 注册只读 `claude_credential_changed` 命令；不需要 capability 改动。
- W6：`src/App.tsx:202-205,421-456,464-522` 在认证错误期间每 60 秒做一次本地凭证检查；变化立即走既有 `fetchClaudeQuota`，相同值保留 60 分钟 API 退避。成功、非认证错误、自动续期未同步、卸载以及在途检查过期时均清除／失效检查定时器，保持最多一个定时器。
- W6 测试：`tests/claude_auth_recovery.test.tsx:242-289` 的 `auth_error_polls_local_credential_expiry_every_minute`、`changed_credential_triggers_immediate_fetch`、`unchanged_credential_waits_for_auth_backoff`、`credential_check_stops_after_success_and_on_unmount`；`src-tauri/src/services/claude.rs:1662-1691` 的 `credential_changed_projection_handles_same_different_and_missing_credentials`（同时断言序列化结果没有令牌字段）。
- W7：`src-tauri/src/services/claude.rs:101-147` 与 `src-tauri/src/services/codex.rs:35-88` 在 `#[cfg(test)]` 下把 `log_msg` 写入内存缓冲；生产分支逐字保留原来的用户日志路径与写入行为。
- W7 测试：`src-tauri/src/services/claude.rs:1693-1702` 与 `src-tauri/src/services/codex.rs:857-866` 的 `test_logging_does_not_touch_user_log_dir`，均实际调用生产同名日志入口并断言测试目标为内存缓冲。

### 验证结果（逐项）

- `cargo fmt --check --manifest-path src-tauri/Cargo.toml` → exit 0。
- `cargo check --offline --locked --tests --manifest-path src-tauri/Cargo.toml` → exit 0（既有 warnings）。
- `cargo test --offline --locked --manifest-path src-tauri/Cargo.toml` → exit 0；483 passed、2 ignored。
- `npx tsc --noEmit` → exit 0。
- `npx vitest run` → exit 0；61 files、713 tests passed。
- `TZ=UTC npx vitest run` → exit 0；61 files、713 tests passed。
- `node --test scripts/*.test.mjs` → exit 0；102 pass、0 fail。
- `node scripts/check_release_version.mjs` → exit 0；release manifests agree on 0.4.14。
- `node scripts/check_latest_request_wiring.mjs` → exit 0。
- `git diff --check` → exit 0。

### 白名单核对

- 本轮修改仅涉及 `src/App.tsx`、`src/services/backend.ts`、`src-tauri/src/{commands.rs,lib.rs,services/claude.rs,services/codex.rs}`、`tests/claude_auth_recovery.test.tsx` 与本报告；均为返工 2 白名单允许的位置。未改 capability、依赖、版本、CHANGELOG、Cargo.toml、Cargo.lock 或 package.json；未运行变异测试、未提交或推送。

### 规格偏差

- 无。

READY_FOR_MUTATION

## §返工 3

### 交付物清单

- X1：`src-tauri/src/services/service_status.rs:271-280` 禁用时仅替换显示状态，先取出再恢复两个 provider 的 `recently_resolved`；`successful_statuses` 的既有基线行为未改。`disable_reenable_then_failure_keeps_recently_resolved`（`:3821-3865`）经 `set_prefs` 和 `poll_once_with_fetchers_and_lookup_at` 的测试辅助路径驱动“成功得到 A → 禁用 → 启用 → 失败”，同时断言快照及经生产 `save_resolved_store` 将写入的列表仍含 A。
- X2：`src-tauri/src/services/claude.rs:1526-1584` 将最终响应状态传到记录器；`src-tauri/src/services/quota_history.rs:85-102` 仅让 `StatusCode::OK` 进入实际追加路径，保留其他 2xx 的 `fetch_quota` 处理。`non_200_success_is_not_recorded`（`:414-424`）驱动生产实际调用的 status gate，断言 206／204 不建文件而 200 写一行。
- X3：`src-tauri/src/services/quota_history.rs:251-263` 每次追加在写 JSON 前收紧为 0600，权限失败直接返回且本条不写；`:348-360` 裁剪临时新文件在写入前设 0600，rename 后保持私有。`existing_world_readable_history_is_tightened_to_0600`（`:569-580`）预建 0644 后追加并断言 0600。
- X4：`src-tauri/src/services/service_status.rs:1707-1718` 以 RFC3339 解析后的时间点倒序；无法解析者排在末尾。`recently_resolved_sorts_by_instant_across_offsets`（`:4059-4088`）验证 `01:00+01:00` 早于 `00:30Z` 且非法时间最后。
- X5：`src-tauri/src/services/quota_history.rs:52-81` 首次／每日裁剪继续在 `spawn_blocking` 中执行；24 小时间隔变为异步等待 channel 的计时桥接，不再占用 Tauri blocking worker。未新增依赖。
- X6：a5 将 `stale_generation_result_does_not_reschedule`（`tests/claude_auth_recovery.test.tsx:346-363`）强化为最新成功排 60 秒后、过期结果 50 秒后才返回，并精确断言第 60 秒的调用次数和时刻；r3c 新增 `failure_carry_drops_entries_older_than_24h`（`service_status.rs:3868-3903`），经生产 poll 失败分支验证 25h 条目丢弃、23h 条目保留。

### 验证结果（逐项）

- `cargo fmt --check --manifest-path src-tauri/Cargo.toml`（`CARGO_TARGET_DIR=/Users/sam/Documents/QuotaBar/src-tauri/target CARGO_INCREMENTAL=0`）→ exit 0。
- `cargo check --offline --locked --tests --manifest-path src-tauri/Cargo.toml`（同上环境）→ exit 0（既有 warnings）。
- `cargo test --offline --locked --manifest-path src-tauri/Cargo.toml 2>&1 | tail -60`（同上环境，前台）→ exit 0；488 passed、2 ignored。
- `npx tsc --noEmit` → exit 0。
- `npx vitest run` → exit 0；61 files、713 tests passed。
- `TZ=UTC npx vitest run` → exit 0；61 files、713 tests passed。
- `node --test scripts/*.test.mjs` → exit 0；102 pass、0 fail。
- `node scripts/check_release_version.mjs` → exit 0；release manifests agree on 0.4.14。
- `node scripts/check_latest_request_wiring.mjs` → exit 0。
- `git diff --check` → exit 0；本轮未运行变异。

### 白名单核对

- 返工 3 的增量修改仅涉及 `src-tauri/src/services/{service_status.rs,claude.rs,quota_history.rs}`、`tests/claude_auth_recovery.test.tsx` 和本报告，均在 spec §7 及返工 2 延续白名单内。工作区中前两轮已有的修改也仅涉及其已报告的允许路径。
- 未改依赖、版本、CHANGELOG、Cargo.toml、Cargo.lock 或 package.json；未联网、未启动应用、未提交或推送。

### 规格偏差

- 开工实测 HEAD／分支为 `dfd130350f2457dd7bbaa07d8917400da9a63237`／`codex/r4-status-history`，Node `v22.23.2`，`/` 可用 3.2 GiB；porcelain 已含前两轮未提交改动及规格目录，并非 spec §8 描述的唯一 specs 条目，按本任务要求以工作区现状继续。
- X5 使用 Tauri re-export 的异步 channel 等待加标准库计时线程作为无新增依赖的等价计时器；裁剪工作本身仍是唯一的 blocking 工作。其余无偏差。

READY_FOR_MUTATION
