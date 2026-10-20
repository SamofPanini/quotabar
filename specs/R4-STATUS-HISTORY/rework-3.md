# R4-STATUS-HISTORY 返工 3（Sol r0 复审后）

Sol 报告与控制面逐条处置见 `specs/R4-STATUS-HISTORY/sol-r0.md`（末节）。下列各条控制面均已回源码核实属实。逐条处理，回填 report.md 新增「§返工 3」节（不删前三节），每条写落实位置（文件:行）与测试名。

## X1（MAJOR·实际行为缺陷）禁用再启用后，首次失败会把已解决列表写空
`ServiceStatusState::set_prefs(false)` 用 `ServiceStatus::unavailable(...)` 覆盖 `runtime.claude/codex`，连同 `recently_resolved` 一起清空；重新启用后若首轮拉取失败，失败分支从这份空快照延续，`poll_once` 随后 `persist_recently_resolved()` ⇒ 文件被写空。
要求：禁用时只替换显示用的状态，**`recently_resolved` 原样保留**（重新启用后照常按 24h 裁剪）；比对基线（`successful_statuses`）的现有行为不变。任何路径都不得因「禁用／重新启用」丢已解决条目。
测试：`disable_reenable_then_failure_keeps_recently_resolved`（经 `set_prefs` 与 `poll_once_with_fetchers_and_lookup_at` 驱动：成功（有已解决 A）→ 禁用 → 启用 → 失败 ⇒ 快照与将写盘的列表都仍含 A）。

## X2（MAJOR）快照只记录 HTTP 200
`claude.rs` 的成功判定是 `response.status().is_success()`（任何 2xx）。要求：**记录快照**只在最终状态码恰为 `200 OK` 时发生；`fetch_quota` 对其他 2xx 的现有处理方式不变（只是不记录）。实现须是生产路径上的真实判定（例如把状态码传给记录函数，由它拒绝非 200），**不得**是以常量调用的恒真包装。
测试：`non_200_success_is_not_recorded`（206／204 不写、200 写；驱动的是生产里被调用的那个判定／记录函数）。

## X3（MAJOR·安全隐私）已存在的 history 文件也要保证 0600
`append_record` 只在 `!path.exists()` 时设权限（另有 exists→open 的 TOCTOU）。要求：每次追加前（或打开后）检查权限，不是 0600 就改为 0600；设权限失败 ⇒ 记日志并**不写入**这一条。裁剪后的新文件同样 0600。
测试：`existing_world_readable_history_is_tightened_to_0600`（预建 0644 文件 → 追加后为 0600）。

## X4（MINOR）已解决事件按时间点排序
`resolved.sort_by(... resolved_at.cmp ...)` 是字符串比较；不同时区偏移下会错。改为解析为时间点后比较（解析失败的排最后）。前端若也有排序，同理。
测试：`recently_resolved_sorts_by_instant_across_offsets`（`01:00+01:00` 早于 `00:30Z`）。

## X5（MINOR）后台裁剪不要占用 blocking worker 睡 24 小时
`quota_history::initialize` 里用 `spawn_blocking` 执行 24h `thread::sleep`。改为异步计时（如 `tokio::time::sleep`；若 crate 无 tokio time 特性，用不新增依赖的等价方式），裁剪本身仍在 `spawn_blocking` 里做。行为不变：启动时一次、此后每 24h 一次。

## X6（测试敏感度）补两条能杀死变异的测试
- **a5 过期结果也重排**仍存活。原因（控制面分析）：现有测试里过期结果在最新结果之后很快返回，变异把定时器改排到的时刻与原时刻几乎相同，断言分辨不出。改为：最新结果成功（排 60 秒后）→ **过期结果在其后 50 秒才返回** → 断言下一次自动拉取发生在第 60 秒（变异会把它推到第 110 秒），并精确断言调用次数。
- **r3c 失败延续路径的 24h 过期**无测试。补 `failure_carry_drops_entries_older_than_24h`：经生产 poll 驱动，失败轮次时一条已解决 25h、一条 23h ⇒ 只剩 23h 那条。

## 不变
spec.md §4／§5／§5b／§8 与 rework-2.md 的白名单照旧（本轮不需要新增白名单；若需要，停下交 `BLOCKED_ALLOWLIST`）。本轮不跑变异（控制面交付后复跑）。末行唯一 verdict：`READY_FOR_MUTATION` 或 `BLOCKED_<大写原因>`。
