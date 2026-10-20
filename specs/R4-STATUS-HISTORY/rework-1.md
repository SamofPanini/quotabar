# R4-STATUS-HISTORY 返工 1（控制面审 r0 后）

r0 三项主体可用（前端重排、快照 schema、已解决 UI 均已核）。以下各条控制面已回源码核实，**按序号逐条处理**；每条都写出落实位置（文件:行）与对应测试名，回填进 report.md 新增的「§返工 1」节（不删 r0 节）。

## W1（MAJOR·实际行为缺陷）拉取失败会抹掉已解决列表与比对基线
现状：`fetch_claude`／`fetch_openai` 失败时返回 `ServiceStatus::unavailable(...)`（incidents 与 `recently_resolved` 均为空），`poll_once_with_fetchers_and_lookup_at` 直接把它写进 `runtime.claude/codex`；`poll_once` 随后 `persist_recently_resolved()` ⇒
- 一次网络抖动就把已解决列表清空，**持久化文件也被写成空**；
- 失败前仍开着的 incident，在下一次成功拉取时以「unavailable 的空 incidents」为比对基线 ⇒ 永远不会进入已解决，悄悄消失。
（`mark_stale_failed_statuses` 的 30 分钟替换同理。）
要求（行为契约）：
1. 某 provider 本轮拉取失败或 `level == unknown` 时，该 provider 的 `recently_resolved` **原样延续**（只做 24h 过期裁剪），持久化文件不得因此变空或丢条目。
2. 「已显示的未解决 incident」比对基线只由**成功**拉取更新：失败期间保留上一次成功拉取的 incidents 作为基线；下一次成功拉取与这份基线比对。
3. 显示层不变：失败时 UI 仍按现状显示 `Status unavailable`（不要求在 unknown 时显示已解决行）。
4. 实现自定（例如 Runtime 里为两个 provider 各存一份「上次成功的 incidents 基线」，失败结果合并入上一份 `recently_resolved`），但不得改变托盘、通知、`provider_delay`、失败计数的现有行为。
具名测试（经 `poll_once_with_fetchers_and_lookup_at` 驱动，不直接调 reconcile）：
- `failure_keeps_recently_resolved_until_expiry`：成功（有已解决 A）→ 失败 ⇒ 快照的 `recently_resolved` 仍含 A；
- `incident_resolved_during_outage_is_detected_after_recovery`：成功（incident X 开着）→ 失败 → 成功（X 不在）⇒ X 进入已解决（时刻按既有优先序）；
- `stale_failure_replacement_keeps_recently_resolved`：覆盖 `mark_stale_failed_statuses` 的 30 分钟替换路径；
- 若持久化写入可经注入路径驱动，加 `failed_poll_does_not_empty_resolved_store`；不可注入则在报告写明理由。
`failed_fetch_does_not_resolve_incidents` 保留（它断言的是「失败不产生新解决」，与本条不矛盾）。

## W2（前端·防御）渲染时按 24h 过滤
后端只在**成功**拉取时裁剪过期条目；状态页长时间拉取失败或 app 睡眠后，过期条目可能仍在快照里。`ServiceStatusNotice` 渲染前按 `resolvedAt`（解析失败视为过期）过滤掉早于「现在 − 24h」的条目；过滤后为空时与「无已解决」等价（含 `return null` 判定）。
测试：`resolved_older_than_24h_is_not_rendered`（含「只剩过期条目 ⇒ 不渲染」）。

## W3（规格 §3.1 违例）裁剪不得在请求路径上做
现状：`record_success_at` → `prune_if_due_at` → `prune_at` 读全文件并重写，发生在 `fetch_quota_once` 的同步路径上（每 24h 一次）。
要求：裁剪移出请求路径（如启动时与每 24h 由后台线程／`spawn_blocking` 执行）；**裁剪与追加必须互斥**（裁剪是「读全文 → 写临时 → rename」，期间的追加不得丢失）——用同一把锁或等价机制。
测试：`append_during_prune_is_not_lost`（可注入：在裁剪读完、rename 之前插入一次追加，断言新行仍在）；`record_success_does_not_prune_inline`（记录调用本身不触发全文件读写，可用注入的计数器断言）。

## W4 变化判定对 `resets_at` 的亚分钟抖动免疫
现状：`previous.windows != record.windows` 逐字比较 `resets_at` 原始字符串。若 API 每次返回的重置时刻带不同的小数秒，每次拉取都会被判为「有变化」而写一行（文件膨胀到每天数百行）。
要求：判定「是否变化」时，`resets_at` 按解析后取整到分钟比较（解析失败退回原字符串比较）；`pct` 仍按原值比较。**写入的记录仍保存原始字符串**（schema 不变）。
测试：`subminute_resets_at_jitter_is_not_a_change`、`resets_at_minute_change_is_a_change`。

## W5 恒真测试与恒真判定
现状：`claude.rs` 以常量调用 `should_record_successful_response(true)`；`cache_hit_stale_and_error_paths_do_not_record` 只是对该恒等函数传 `true`/`false`，删掉生产调用点也不会有任何测试失败（控制面变异 q4 实测，见下）。
要求：删除 `should_record_successful_response` 与该恒真测试；二选一：
- (a) 若 `claude.rs` 已有可注入「真实响应 vs 缓存命中 vs stale 回退」的测试缝，用它写 `fresh_200_records_and_cache_hit_does_not`，经 `fetch_quota`／`fetch_quota_once` 驱动；
- (b) 没有就**不要新造恒真测试**：在报告写明「仅在真实 200 解析后记录」由结构保证，并列出缓存命中、stale 回退、各错误路径各自 `return` 的行号。
不允许为了 (a) 重排 `fetch_quota` 的控制流。

## 变异补测（控制面 r0 变异结果，见 mut_r4 摘要；存活项须补到能被杀）
控制面在 r0 上跑了 18 个变异（脚本 `~/codex-claude-runs/qb-ping1/mut_r4.py`，只读参考，不要运行），14 个被杀、4 个存活：
- **a4 异常路径不重排**（删掉 `catch` 分支里的 `scheduleClaudePollRef.current(...)`）存活 ⇒ 现有测试没有让 `backend.getQuota()` **抛异常**。补 `thrown_error_reschedules_with_its_interval`：抛异常后下一次自动拉取按 `getClaudeRefreshIntervalMs(message)` 的间隔发生，且只发生一次。
- **a5 过期结果也重排**（过期返回时改为调用重排后再 return）存活 ⇒ `stale_generation_result_does_not_reschedule` 没有区分「过期结果」与「最新结果」各自排的定时器。补强该测试：让过期结果在最新结果**之后**返回，并断言之后的自动拉取时间点只由最新结果决定（精确断言调用次数与时刻）。
- **r6 已解决按旧→新排序**存活 ⇒ 补 `recently_resolved_is_sorted_newest_first`（至少两条、乱序输入）。
- **q4 删掉 `claude.rs` 的生产调用点**存活 ⇒ 见 W5。


## 不变
spec.md §4／§5／§5b／§7／§8 全部照旧（白名单、禁止项、验证命令）。本轮不跑变异（控制面交付后复跑）。
交付：report.md 增「§返工 1」节；末行唯一 verdict：`READY_FOR_MUTATION` 或 `BLOCKED_<大写原因>`。
