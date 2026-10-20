## 交付物清单

结论：X1–X3 已实质修复；X4、X5 的核心缺陷已修复，但各留一项 MINOR 边界问题。按 spec §10，没有 BLOCKER 或需返工的 MAJOR，可以收敛批准。

- **X1：通过。** 禁用时只替换显示状态，并恢复两个 provider 的 `recently_resolved`；`successful_statuses` 未改。
- **X2：通过。** 最终 HTTP 状态被传入实际记录函数，非 `200 OK` 在追加前返回。
- **X3：通过。** 每次追加都在序列化前设置 0600；权限设置失败会返回错误、不写该记录。
- **X4：主体通过，有一项 MINOR。**
- **X5：主体通过，有一项 MINOR。**
- **X6：通过；测试与 r3 变异读数一致。**

发现：

1. **MINOR — 从旧持久化文件恢复后，失败路径不会重新按时间点排序。**  
   时间点排序只发生在 [reconcile_recently_resolved](</Users/sam/Documents/Explore/project/quotabar dev/quotabar-r4-status-history/src-tauri/src/services/service_status.rs:1707>)；[carry_recently_resolved](</Users/sam/Documents/Explore/project/quotabar dev/quotabar-r4-status-history/src-tauri/src/services/service_status.rs:1721>) 和加载路径只过滤、不排序。  
   反例：升级前的文件已按旧字符串规则错误排序 → 启动恢复 → 首次拉取失败。列表仍保持旧顺序，直到下一次成功 reconcile。失败状态下当前 UI 显示 `Status unavailable`，因此影响有限，按 §10 记 MINOR。

2. **MINOR — X5 的调度初始化不幂等，休眠线程也不可取消。**  
   每次调用 [initialize](</Users/sam/Documents/Explore/project/quotabar dev/quotabar-r4-status-history/src-tauri/src/services/quota_history.rs:52>) 都启动一条永久异步循环；每轮又由 [sleep_until_next_prune](</Users/sam/Documents/Explore/project/quotabar dev/quotabar-r4-status-history/src-tauri/src/services/quota_history.rs:74>) 创建一个休眠 24 小时的 detached OS 线程。  
   生产当前只有 [lib.rs](</Users/sam/Documents/Explore/project/quotabar dev/quotabar-r4-status-history/src-tauri/src/lib.rs:91>) 一个调用点，所以不会正常触发重复；若同一进程多次初始化，则会累计循环和休眠线程。运行时退出时接收端可被丢弃，但休眠线程不能提前取消；进程真正退出时由 OS 终止。原来的 blocking-pool 占用已修复，因此按 MINOR follow-up 处理。

## 验证结果（逐项）

1. X1

最强反例：已有 resolved A，同时成功基线仍含 active X → 禁用期间有旧 poll 在途 → 重新启用后首次失败 → 再次成功且 X 消失。

- 禁用会增加 epoch，使旧 poll 结果失效。
- 当前显示状态仍保留 A。
- 首次失败从当前状态携带 A并执行 24h 裁剪。
- `successful_statuses` 保持禁用前基线，恢复成功后仍能识别 X 已解决。
- [disable_reenable_then_failure_keeps_recently_resolved](</Users/sam/Documents/Explore/project/quotabar dev/quotabar-r4-status-history/src-tauri/src/services/service_status.rs:3821>) 覆盖核心禁用→启用→失败路径。
- 未发现 X1 回归。

2. X2

最强反例：初次请求认证失败，重读凭证后的最终响应为 `206 Partial Content`，且带有效额度 JSON。

- [claude.rs](</Users/sam/Documents/Explore/project/quotabar dev/quotabar-r4-status-history/src-tauri/src/services/claude.rs:1526>) 保存的是重试后的最终状态。
- UI 对其他 2xx 的既有解析行为不变。
- [record_success_at_status](</Users/sam/Documents/Explore/project/quotabar dev/quotabar-r4-status-history/src-tauri/src/services/quota_history.rs:93>) 明确只接受 `StatusCode::OK`。
- 全仓静态搜索确认 `record_success` 只有一个生产调用点，签名已同步更新；没有遗漏调用点。
- `non_200_success_is_not_recorded` 直接驱动生产使用的状态 gate，200/204/206 判定正确。

3. X3

最强反例：预建 0644 history 文件后发生一次需要追加的额度变化。

- 文件打开后、JSON 写入前执行 chmod 0600。
- chmod 失败时 `?` 立即返回，既不序列化也不更新内存中的 `last`，后续请求仍可重试。
- 新建文件若 chmod 失败，可能留下空文件，但不会写入额度数据。
- 裁剪临时文件也在首行写入前设为 0600。
- `existing_world_readable_history_is_tightened_to_0600` 覆盖既有宽权限文件。
- 未发现达到 MAJOR 门槛的权限回归。

4. X4

最强反例：`01:00+01:00`、`00:30Z` 与非法时间同时出现。

- 解析后的 `DateTime` 按真实 instant 倒序；结果为 `00:30Z`、`01:00+01:00`、非法时间。
- 测试同时覆盖跨 offset 和非法值置后。
- 主成功路径修复正确；旧文件恢复后首轮失败不重排是上述 MINOR。

5. X5

最强反例：同一进程连续调用 `initialize()` 两次，再立即关闭 app。

- 裁剪本身仍在 `spawn_blocking` 中；24h 等待不再占 blocking worker。
- 两次初始化会产生两条调度循环，但共享 `last_pruned_at` 会阻止同一时刻重复裁剪文件。
- 两个休眠 OS 线程仍会存在至计时结束或进程退出；无取消和单例 guard，记 MINOR。
- 当前生产调用点唯一，因此没有正常启动路径上的重复初始化回归。

6. X6 与变异

- `stale_generation_result_does_not_reschedule` 现在让旧结果延迟 50 秒返回；若旧结果错误重排，会把 60 秒自动拉取推迟，断言能够区分。r3 中 a5 为 `killed`，一致。
- `failure_carry_drops_entries_older_than_24h` 经生产 poll 失败分支输入 25h/23h 两条，只保留 23h；r3 中 r3c 为 `killed`，一致。
- r3 共列 32 项：30 项 killed、r6 与 q4 两项 SKIP，无 SURVIVED；“有效杀死 30/32”与表格一致。
- X1–X4 对应变异均为 killed。

只读静态验证：

- `git diff --check dfd1303 --`：exit 0；仍有沙箱无法创建 `/tmp/xcrun_db-*` 的环境警告。
- 遵守禁令，未运行 cargo/npm/node；报告中的 `488 passed、2 ignored` 和 `713 vitest passed` 未由本线程重新执行。
- 未修改任何文件。

## 规格偏差

- X4 的恢复后失败路径排序、X5 的重复初始化与退出取消没有违反 §10 的收敛门槛，作为 MINOR follow-up，不阻挡批准。
- 无白名单外改动。
- 本复核单的措辞没有影响判断方向；它只限定了本轮检查范围，并促使我重点验证初始化生命周期、权限失败和调用点完整性。结论由当前生产代码、测试及 r3 变异记录共同决定。

SOL_APPROVE
---
## 控制面逐条处置（2026-10-20 12:01）
| # | Sol 发现 | 回代码核 | 处置 |
|---|---|---|---|
| 1 | MINOR：旧持久化文件恢复后首次失败不重排 | 属实：排序只在 `reconcile_recently_resolved`，`carry_recently_resolved` 只过滤 | 控制面小修：抽出 `sort_recently_resolved`，carry 路径也调用；新测试 `failure_carry_resorts_restored_entries_by_instant`（经生产 poll 失败分支）。定点变异「carry 不重排」killed |
| 2 | MINOR：`quota_history::initialize` 重复调用会叠加调度循环 | 属实（生产调用点唯一，正常路径不触发） | 控制面小修：进程级 `AtomicBool` 守卫，只启动一条调度循环。休眠线程不可取消＝进程退出由 OS 回收，接受。守卫无测试（进程级静态量，测试会跨用例互相污染），属结构保证 |
| — | r3 变异 r6 SKIP（X4 改写后锚点消失） | 属实 | 新锚点定点变异 `right.cmp(&left)`→`left.cmp(&right)` killed |

修后全量：cargo fmt --check OK；Rust 489 passed／2 ignored；vitest 713 passed（61 files）；tsc OK；release:check 0.4.15 一致。
