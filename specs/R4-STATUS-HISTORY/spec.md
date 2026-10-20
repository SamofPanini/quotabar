# R4-STATUS-HISTORY｜Claude 轮询定时器重排 ＋ 状态页故障解决后保留 24h ＋ Claude 额度快照记录

基线 fork main `dfd1303`（0.4.14）。三项互相独立，可分段实现；任一项被白名单挡住时，其余两项照常交付并在报告写明。

## 0. 现状（控制面已核；你须回源码复核，有出入就报）

### 0.1 Claude 自动刷新会被一个长定时器卡住（Sam 10-19 实测：ping 恢复后要手动刷新才更新）
- `src/App.tsx` 约 441–464 行：自动轮询是 `run → await fetchClaudeQuota() → setTimeout(run, claudeIntervalRef.current)`，**拉完一次再按当时的间隔排下一次**。
- `fetchClaudeQuota`（约 407–439 行）按结果改 `claudeIntervalRef.current`：成功＝`AUTO_REFRESH_INTERVAL_MS`(60s)；错误经 `getClaudeRefreshIntervalMs`（`src/services/app_state.ts` 约 194–212 行）：429＝5 min，认证错误＝`AUTH_REFRESH_INTERVAL_MS`(60 min)。
- 轮询之外还有三处调 `fetchClaudeQuota`：`refreshClaudeQuota`（手动刷新，约 728–740 行）、`refreshAfterPing`（约 851–858 行）、启动时的 effect。它们会把间隔改回 60s，**但已经排好的那个 setTimeout（可能是 60 min）不会被取消重排** ⇒ 认证失效后即使 ping 恢复、手动刷新成功，下一次自动刷新仍要等旧定时器到点。这就是要修的缺陷。
- 后端 `claude.rs` 的 120s 响应缓存（`QUOTA_CACHE_TTL`）与本缺陷无关，**不改**。
- `scripts/check_latest_request_wiring.mjs` 第 6–9 行登记了 `src/App.tsx` 的 `fetchClaudeQuota`／`claude_request_generation`：改它的结构或调用签名前先读该检查器，必要时只做让检查器继续通过的最小调整（检查器本身在白名单内，但不得放宽它的判据）。

### 0.2 状态页故障一解决，提示整块消失
- 后端 `src-tauri/src/services/service_status.rs`：`build_status`（约 1268 行起）用 `relevant(event, true)` 丢弃 `resolved`/`postmortem` 的事件；Claude 的 `summary.json` 本来就只列未解决事件 ⇒ 解决后事件从 `incidents` 消失。
- 前端 `src/components/ServiceStatusNotice.tsx` 第 7 行：`level === 'operational'` 且无 incidents/maintenances 时 `return null` ⇒ 整块不见。Sam 要的是：**解决后 24 小时内仍显示，并显示具体的解决时间**。
- 控制面 2026-10-20 实测两个公开端点都可用、都带 `resolved_at`：`https://status.claude.com/api/v2/incidents.json`（约 190 KB，50 条）、`https://status.openai.com/api/v2/incidents.json`（约 33 KB，25 条；id 为 ULID，与 proxy/summary 的 id 是否一致**未核**）。均小于现有 `MAX_RESPONSE_BYTES`(1 MB)。
- 现有「解决」通知（`{provider}: resolved`，约 588 行）与托盘告警逻辑保持不变。

### 0.3 Claude 额度没有任何历史
- `claude.rs` `fetch_quota`（约 1243 行起）在真正收到 200 并解析后（约 1478–1495 行，`[Quota] SUCCESS` 日志处）得到 `five_hour`／`seven_day`／`seven_day_opus`／`seven_day_sonnet`／`seven_day_omelette`／Fable 窗口；这些值只给 UI，不落盘。Sam 需要事后对照「5h 额度用了多少 ⇔ 周额度涨了多少」（10-19 曾一个 5h 窗口把周额度从 84% 推到 99%，无从回查）。
- 读取方＝vault 的 `usage_ledger.py`（控制面维护），**schema 以 §3 为准，不得增减键名**。

## 1. Claude 轮询重排（前端）
1. 行为契约：**任何一次 Claude 拉取完成**（轮询、手动刷新、ping 后刷新、启动）之后，下一次自动拉取都按**这次结果**的间隔重新排定；此前已排的定时器作废。任一时刻最多只有一个待触发的 Claude 自动刷新定时器。
2. 成功 ⇒ 60s 后再拉；错误 ⇒ 仍按 `getClaudeRefreshIntervalMs`（429＝5 min，认证错误＝60 min）——**退避语义不变**，只是由最近一次结果决定。
3. 被 generation 判为过期的结果（`claude_request_generation.isCurrent` 为假）不得重排定时器。
4. 卸载／`claudeAutoRenewSynced` 变化时清掉定时器，不得残留。
5. 实现方式自定（如把定时器句柄提到 ref、在 fetch 完成处统一重排），但不得引入第二条轮询循环。

## 2. 状态页故障解决后保留 24h（后端为主，前端显示）
1. **跟踪**：每个 provider 记住上一次成功拉取时「已显示的未解决 incident」（`build_status` 产出的 `incidents`，按 id）。某次**成功**拉取（`error.is_none()`）中，之前在、现在不在（或以 resolved/postmortem 出现）⇒ 视为已解决。拉取失败、`level == unknown` 时**不做任何解决判定**。只跟踪 incidents，不跟踪 maintenances。
2. **解决时刻**，按优先序：
   a. 该事件在本次 feed 中以 resolved/postmortem 出现且带 `resolved_at` ⇒ 用它（`RawEvent` 可加 `#[serde(default)] resolved_at`）；
   b. 否则在本轮内对该 provider 的 `incidents.json`（§0.2 两个 URL）发**至多一次** GET（沿用 `shared_http_client`、现有超时与 `MAX_RESPONSE_BYTES`），按 id 匹配取 `resolved_at`；
   c. 仍取不到 ⇒ 用本次发现消失的时刻，并标 `resolvedAtEstimated: true`。
   查询失败只记日志，不影响本轮状态与 `error` 字段。查询必须可注入（测试不联网）。
3. **数据**：`ServiceStatus` 增加 `recently_resolved: Vec<ResolvedServiceEvent>`（serde camelCase ⇒ `recentlyResolved`；`#[serde(default)]`），元素＝事件最后一次未解决时的 `ServiceEvent`（`alert` 置 `None`）＋ `resolved_at: String`（RFC3339）＋ `resolved_at_estimated: bool`。按 `resolved_at` 新→旧排序；`resolved_at` 早于「现在 − 24h」的移除。同一 id 再次以未解决出现 ⇒ 从 `recently_resolved` 移除、回到 `incidents`。
4. **不得影响**：`level`、`alert`、`is_incident()`／`is_active()`、托盘状态、通知判定与指纹、`provider_delay`。即：只有 `recently_resolved` 不同的两个快照，对托盘与通知完全等价。
5. **持久化**：`recently_resolved` 存到 `state_location::primary_state_dir()/service-status-resolved.json`（`~/.config/quotabar-custom/`），启动时读回（丢弃已过 24h 的）；写入走临时文件＋rename，权限 0600；文件损坏或读不到 ⇒ 记日志、按空处理，不得影响状态拉取。路径可注入（测试用临时目录，`temp_dir()` 下先 `canonicalize()` 再 join）。
6. **前端**（`ServiceStatusNotice.tsx`、`src/services/service_status.ts`、类型）：
   - 有未解决事件时：现有显示逐字不变；其下追加已解决事件（见下）。
   - 只有已解决事件时：不再 `return null`，显示一个**中性、降亮度**的提示块（不用 warning/critical 的颜色与圆点颜色）。
   - 每条已解决事件一行：`[Resolved] {name}`（↗ 链接沿用现有按钮），第二行 `Started {开始} · Resolved {解决}`，时间用现有 `formatServiceStatusDate`；`resolvedAtEstimated` 时写 `Resolved by {时间}`。
   - 数量：未解决沿用现有最多 2 条；已解决最多 2 条，多出部分并入现有的 `+N more on the status page`。
   - `level === 'unknown'` 的 `Status unavailable` 逻辑不变。

## 3. Claude 额度快照记录（后端）
1. 新模块（如 `src-tauri/src/services/quota_history.rs`），在 §0.3 的成功解析处调用。**只记录真实收到的 200 响应**：响应缓存命中、stale 回退、错误、`disconnected` 一律不记。记录失败只写日志，绝不改变 `fetch_quota` 的返回值或耗时特征（不得在请求路径上做全文件扫描）。
2. 文件：`state_location::primary_state_dir()/quota-history.jsonl`，追加写，权限 0600，一行一个 JSON 对象，行尾 `\n`。路径可注入。
3. **每行 schema（v1，读取方定义，键名逐字）**：
   ```json
   {"v":1,"t":"2026-10-19T11:41:05Z","provider":"claude",
    "windows":{"five_hour":{"pct":12.0,"resets_at":"2026-10-19T13:00:00+00:00"},
               "seven_day":{"pct":84.0,"resets_at":"..."}}}
   ```
   - `t`＝收到响应的 UTC 时刻（RFC3339，秒精度，`Z` 结尾）。
   - `windows` 的键取自 API 原始键名：`five_hour`、`seven_day`、`seven_day_opus`、`seven_day_sonnet`、`seven_day_omelette`、`fable5`（Fable 窗口，无论来自 `FABLE5_QUOTA_KEYS` 还是 weekly_scoped）。**只写本次响应里存在的窗口**；`pct`＝utilization 原值（0–100 浮点），`resets_at`＝原始字符串或 `null`。
4. **何时写**：与上一条已写记录相比，任一窗口的 `pct` 或 `resets_at` 不同（含窗口出现／消失）⇒ 写；完全相同但距上一条已写记录 ≥ 30 分钟 ⇒ 也写一条（心跳，用来区分「没变」与「没在跑」）。上一条记录在启动时从文件末行读回；末行损坏 ⇒ 当作没有上一条。
5. **保留期**：进程启动时与此后每 24h 至多一次，删除 `t` 早于「现在 − 120 天」的行（临时文件＋rename；不能解析的行一并丢弃并在日志记数）。
6. 不新增 Tauri command、不改 UI。

## 4. 设计意图
不新增常驻元素。已解决事件只在它确实存在的 24h 内出现，用降亮度的中性样式与「Resolved」字样跟进行中的故障区分；24h 后自动消失。轮询修复与快照记录对界面零改动。Apple 式检验：已解决提示若是 Apple 来做，会以灰度的一行「已恢复」出现在原处，而不是一个新面板或弹窗。

## 5. 必须保留
- Claude：`claude_request_generation` 的过期结果丢弃语义；`keepClaudeQuotaOnError`；认证错误时的 `refreshClaudeLogin` 流程；429 退避与后端 `remember_rate_limit`；120s 响应缓存。
- 状态页：现有托盘告警、通知（含 `{provider}: resolved` 标题）、proxy 优先（含 resolved）的合并规则、`provider_delay` 与失败计数、`MAX_RESPONSE_BYTES`。
- 既有测试不得删除或放宽。

## 5b. 不做
不改版本号与 CHANGELOG（控制面合并时补）；不改 Codex 的轮询与额度拉取；不记录 Codex 快照；不做历史图表 UI；不碰 Ping、托盘图标、capability、release 脚本、Cargo.toml／Cargo.lock、package.json；不联网。

## 6. 测试（具名；直接驱动生产函数；夹具照后端真实形状构造）
前端 vitest（新文件 `tests/claude_poll_reschedule.test.tsx`，可用 fake timers 渲染 App 或抽出的生产 hook；报告列全名）：
- `auth_error_then_ping_refresh_reschedules_to_one_minute`：轮询拿到认证错误（排 60 min）→ 触发 ping 后刷新且成功 → 推进 60s 内必有一次新的 `getQuota` 调用（而不是等 60 min）。
- `manual_refresh_success_cancels_pending_backoff_timer`：429（排 5 min）→ 手动刷新成功 → 60s 后自动拉取；且此后 5 min 那个旧定时器不再额外触发一次（调用次数精确断言）。
- `out_of_loop_error_reschedules_with_backoff`：手动刷新返回 429 ⇒ 下一次自动拉取在 5 min 后，不在 60s 后。
- `stale_generation_result_does_not_reschedule`：过期结果返回时不改变已排的定时器。
- `unmount_clears_claude_poll_timer`：卸载后推进任意时间不再调用 `getQuota`。
前端 vitest（`tests/service_status_ui.test.tsx` 增补或新文件）：
- `resolved_only_renders_neutral_notice_with_resolved_time`：`level: operational`、incidents 空、`recentlyResolved` 1 条 ⇒ 渲染、无 warning/critical class、含 `Resolved` 与格式化时间。
- `estimated_resolution_says_resolved_by`。
- `active_and_resolved_render_active_first_and_cap_two_each`。
- `no_resolved_and_operational_still_renders_nothing`（现状保持）。
Rust（`service_status.rs` tests 模块）：
- `incident_disappearing_on_success_moves_to_recently_resolved`
- `failed_fetch_does_not_resolve_incidents`
- `resolved_at_prefers_feed_then_incidents_lookup_then_observed`（三种来源各一个断言；lookup 用注入的假查询）
- `resolved_entries_expire_after_24h`
- `reopened_incident_leaves_recently_resolved`
- `recently_resolved_does_not_change_alert_level_or_notifications`
- `resolved_store_round_trips_and_ignores_corrupt_file`（临时目录先 `canonicalize()`）
Rust（`quota_history.rs` tests 模块）：
- `writes_line_with_v1_schema_on_fresh_success`（逐键断言 §3 schema，含只写存在的窗口）
- `identical_values_within_30_minutes_are_not_rewritten`
- `heartbeat_written_after_30_minutes`
- `cache_hit_stale_and_error_paths_do_not_record`（经 `fetch_quota` 的可注入路径或抽出的判定函数——**判定函数必须被生产路径实际调用**）
- `prune_drops_lines_older_than_120_days_and_unparseable_lines`
- `history_file_is_0600`
**本轮不跑变异**（控制面交付后跑，并会把变异打到编排上：重排、解决判定、写入判定）。

## 7. 白名单
`src/App.tsx`、`src/services/app_state.ts`、`src/components/ServiceStatusNotice.tsx`、`src/services/service_status.ts`、`src/types/*.ts`、`src/redesign/panels.css`、`src/styles/*.css`、`src-tauri/src/services/service_status.rs`、`src-tauri/src/services/claude.rs`（仅 §3 的调用点与必要的注入）、`src-tauri/src/services/quota_history.rs`（新）、`src-tauri/src/services/mod.rs`（仅注册新模块）、`src-tauri/src/lib.rs`（仅当启动时需要初始化／裁剪）、`src-tauri/src/domain/models.rs`（仅当类型在此）、`scripts/check_latest_request_wiring.mjs` 与其 `.test.mjs`（仅当 §1 必须调整，且不得放宽判据）、`tests/**`、`specs/R4-STATUS-HISTORY/report.md`。确需改其他文件 ⇒ 不改，报告写明，verdict 用 `BLOCKED_ALLOWLIST`。不新增依赖。

## 8. 本地验证（只允许）
- 开工自检：`git rev-parse HEAD`＝`dfd130350f2457dd7bbaa07d8917400da9a63237`；分支 `codex/r4-status-history`；`git status --porcelain` 恰为 1 条 `?? specs/R4-STATUS-HISTORY/`；node v22.x；`df -h /`（< 2.5 GB 即停，verdict `BLOCKED_DISK`）。
- `export CARGO_TARGET_DIR=/Users/sam/Documents/QuotaBar/src-tauri/target CARGO_INCREMENTAL=0`；`cargo fmt --check --manifest-path src-tauri/Cargo.toml`；`cargo check --offline --locked --tests --manifest-path src-tauri/Cargo.toml`；`cargo test --offline --locked --manifest-path src-tauri/Cargo.toml 2>&1 | tail -60`（全量，前台，超时 900 秒；fork 自身 crate 的编译产物已被清掉，首次重编几分钟属正常）。
- `npx tsc --noEmit`；`npx vitest run`；`TZ=UTC npx vitest run`；`node --test scripts/*.test.mjs`；`node scripts/check_release_version.mjs`；`node scripts/check_latest_request_wiring.mjs`。
- 禁止：联网、git commit／push、启动应用；运行 `claude`／`codex`／`ccstats`／`pgrep`；读 keychain、`~/.claude*`、`~/.codex*`、`~/.config`、`~/Library`（`~/.cargo/registry` 源码可只读查阅）。

## 9. 交付
`specs/R4-STATUS-HISTORY/report.md`：§0 复核结论（含与本单出入）；§1–§3 每条落实位置（文件:行）；「测试 → 测试名」表（只列真实存在的名字）；全量 cargo test 与 vitest 结果行与 exit；其余检查 exit；白名单核对；偏差如实写。**本单里的数、行号与你实测有出入就报，不要去凑。**
末行唯一 verdict：`READY_FOR_MUTATION` 或 `BLOCKED_<大写原因>`。

## 10. 复审收敛规则
只有 BLOCKER，以及属于「实际行为缺陷／安全或隐私缺陷／CI 必败」的 MAJOR 要求返工；测试敏感度不足记 follow-up。「必须保留」被削弱按实际缺陷处理。
