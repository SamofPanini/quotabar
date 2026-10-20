## 交付物清单

- 完成基于 `dfd1303` 的全部未提交改动、新文件及 R4 规格、两轮返工与变异结果的只读复核。
- 未修改文件、未执行 git 写操作、未联网，未读取受禁目录。
- 结论：存在 3 项需要返工的 MAJOR，不能批准。

发现：

1. **MAJOR（实际行为缺陷）— 禁用再启用后的首次失败会写空 resolved 持久化。**  
   [service_status.rs](</Users/sam/Documents/Explore/project/quotabar dev/quotabar-r4-status-history/src-tauri/src/services/service_status.rs:258>) 禁用时用 `ServiceStatus::unavailable` 清掉两个 provider 当前快照中的 `recently_resolved`；重新启用后的首轮若失败，[失败延续逻辑](</Users/sam/Documents/Explore/project/quotabar dev/quotabar-r4-status-history/src-tauri/src/services/service_status.rs:630>) 从这个已清空的当前快照继承，随后 [poll_once](</Users/sam/Documents/Explore/project/quotabar dev/quotabar-r4-status-history/src-tauri/src/services/service_status.rs:352>) 将空列表写盘。若这时退出，文件中的条目永久丢失。  
   反例：已有 resolved A → 禁用状态检查 → 重新启用 → Claude/OpenAI 首次拉取失败 → `service-status-resolved.json` 被写成空列表。

2. **MAJOR（实际行为缺陷）— 快照并非“只记录真实 200”。**  
   [claude.rs](</Users/sam/Documents/Explore/project/quotabar dev/quotabar-r4-status-history/src-tauri/src/services/claude.rs:1526>) 使用 `response.status().is_success()`，会接受所有 2xx；只要例如 `206` 返回可解析额度数据，便会到达 [record_success](</Users/sam/Documents/Explore/project/quotabar dev/quotabar-r4-status-history/src-tauri/src/services/claude.rs:1584>)。实现报告和 mutation 的“只有真实 200 能到达”结论不成立，应显式要求 `StatusCode::OK`。

3. **MAJOR（安全隐私）— 不能保证已有 history 文件为 0600。**  
   [quota_history.rs](</Users/sam/Documents/Explore/project/quotabar dev/quotabar-r4-status-history/src-tauri/src/services/quota_history.rs:233>) 只在 `!path.exists()` 时 chmod。预先存在的 0644 文件会直接追加敏感额度记录；此外 `exists → open` 有 TOCTOU，若首次 chmod 失败，留下的文件在后续追加时也不会再修复权限。现有权限测试只覆盖新建文件。

4. **MINOR — 已解决事件按 RFC3339 原字符串排序，不是按时间点排序。**  
   [service_status.rs](</Users/sam/Documents/Explore/project/quotabar dev/quotabar-r4-status-history/src-tauri/src/services/service_status.rs:1703>) 的字典序在不同时区偏移下会错误。例如 `01:00+01:00` 实际早于 `00:30Z`，却会排在前面。

5. **MINOR — 后台裁剪永久占用一个 blocking worker。**  
   [quota_history.rs](</Users/sam/Documents/Explore/project/quotabar dev/quotabar-r4-status-history/src-tauri/src/services/quota_history.rs:62>) 用 `spawn_blocking` 执行 24 小时的 `thread::sleep`。它不会产生多个循环任务，但会长期占用 blocking-pool 线程，并可能拖延运行时关闭。裁剪与追加在单进程内由同一 mutex 覆盖 read→rename，未发现丢追加的竞态。

6. **MAJOR（测试敏感度；按 §10 仅作 follow-up）— 两个关键变异仍存活。**  
   `mutation.md` r2 明确记录 `a5 过期结果也重排` 与 `r3c 失败延续路径改为 48h` 存活。现有测试名称虽符合任务单，但敏感度仍不足；这两项本身不单独阻挡收敛，生产代码静态核查也分别显示 generation guard 与 24h cutoff 正确。

## 验证结果（逐项）

1. 轮询定时器

- quota 自动轮询本身最多一个待触发 timer：[调度函数](</Users/sam/Documents/Explore/project/quotabar dev/quotabar-r4-status-history/src/App.tsx:475>) 总是先清旧句柄。
- 拉取中发生手动刷新／ping：后启动请求提升 generation，先前请求无论先后返回均不能改状态或重排。
- 过期结果晚于最新结果返回：[成功和异常路径](</Users/sam/Documents/Explore/project/quotabar dev/quotabar-r4-status-history/src/App.tsx:418>) 都在调度前检查 generation。
- `claudeAutoRenewSynced` 翻转或卸载时清除 quota timer、凭证检查 timer，并把调度 ref 替换为 no-op；在途请求不会留下 timer，但后端请求本身不会被取消。
- 认证错误状态按 W6 会同时存在一个 60 分钟 quota timer 和一个 60 秒本地凭证检查 timer；因此若“任一定时器”按字面统计，答案不是一个，但两类 timer 各自最多一个，且 W6 明确授权这种并存。
- 429 仍为 5 分钟；普通认证过期仍为 60 分钟。原有 signed-out 特例仍为 60 秒，没有被本轮改动。
- 没有发现旧 timer 重复触发的生产缺陷，但 a5 变异存活，测试需增强。

2. 已解决保留

- 成功／失败／unknown／30 分钟 stale 穿插：常规路径正确；`successful_statuses` 只由成功且非 unknown 的结果更新，失败保留最近成功 incident 基线。
- 故障在失败期间解决：恢复后的首次成功会与保留基线比较并转成 resolved。
- 重开：相同 id 再次活跃会从 `recently_resolved` 移除。
- 启动恢复后首次拉取失败：恢复列表在当前快照中，失败会继续携带并按 24h 裁剪。
- 两 provider 一成一败：分别 reconcile/carry，不会相互覆盖。
- 24h 边界使用 `>= cutoff`：恰好 24h 保留，更早才删除；前端相同。
- `resolved_at` 来源优先序正确：feed → 每 provider 至多一次 incidents lookup → 观察时刻并标 estimated。
- lookup 失败只记日志并走 estimated，不污染 status `error`。
- 正常情况下写空仅发生在全部条目过期、事件重开、文件损坏／读不到后按空恢复；另有上述禁用→启用→首轮失败的非预期写空缺陷。
- `recently_resolved` 不参与 `is_incident`、`is_active`、`alert`、通知 fingerprint/event fingerprint 或 `provider_delay`；托盘、通知标题及 `{provider}: resolved` 语义保持不变。
- 唯一额外问题是按字符串而非时间点排序。

3. 额度快照

- 缓存命中、请求错误、429 stale、认证 stale、解析错误及 disconnected 均在记录点前返回；这些路径不记录。
- 非 200 的普通错误不记录，但所有可解析 2xx 都会记录，违反“只记录真实 200”。
- schema 字段及大小写与 §3.3 一致：`v/t/provider/windows`、窗口原始键、`pct/resets_at`；`t` 为秒精度 UTC `Z`，只序列化存在且有数值 utilization 的窗口。
- 窗口出现／消失、`pct` 变化、30 分钟心跳正确；`resets_at` 比较解析后按分钟，写盘仍保留原字符串。
- 启动尾行损坏会视为无上一条；后台裁剪会删除损坏行和超过 120 天的行。
- 单进程内裁剪与追加互斥，测试实际覆盖 rename 前并发追加。
- 0600 只对新建或裁剪后的文件可靠，对既有文件不可靠。
- 后台裁剪没有重复任务或单进程丢追加问题，但 24 小时 blocking sleep 是资源问题；互斥也不覆盖多进程同时运行。

4. 测试、§5 与 CI

- App 定时器测试实际渲染生产 `App`；W1 测试经生产 poll 路径；裁剪并发测试驱动真实 append/prune 函数。未发现原 W5 那种常量恒真判定重新出现。
- 快照记录点缺少可杀死“非 200 也记录”或删除生产调用点的集成测试；schema 测试也未逐项断言所有顶层/窗口键和 `resets_at: null`。
- §5 项目静态核对未见削弱：generation、`keepClaudeQuotaOnError`、手动认证刷新、429 `remember_rate_limit`、120 秒缓存、proxy 合并、托盘/通知、provider delay、失败计数和响应大小限制仍在。
- 白名单：全部改动均落在 spec §7 加 rework-2 追加白名单内。
- `git diff --check dfd1303 --`：exit 0；环境同时输出 `/tmp/xcrun_db-*` 无法创建的只读沙箱警告。
- 按本复核禁令，未运行 cargo/npm/node，因此不能独立确认报告所称 `483 Rust tests / 713 vitest`、fmt、类型检查或时区测试。静态检查没有发现确定的 CI 必败项；非 Unix 平台不会验证或实施 0600，Windows rename 覆盖语义也未被测试。

## 规格偏差

- 审查过程无文件改动、无越权读取、无禁用命令。
- 本复核单的措辞没有影响判断方向；它影响的是反例覆盖范围和严重级别分类。上述结论同时包含通过项和失败项，均由规格与当前代码路径交叉核得。
- 由于存在属于“实际行为缺陷”和“安全隐私”的 MAJOR，按 spec §10 必须返工；建议至少补充禁用/启用失败持久化、严格 200、已有宽权限文件三类回归测试。

SOL_CHANGES_REQUIRED
---
## 控制面逐条处置（2026-10-20 04:12）
| # | Sol 发现 | 回代码核 | 处置 |
|---|---|---|---|
| 1 | 禁用再启用后首次失败写空持久化 | 属实：`set_prefs(false)` 以 `ServiceStatus::unavailable` 覆盖 `runtime.claude/codex`（含 `recently_resolved`），失败分支从空快照延续，`poll_once` 随即写盘 | 返工 3 · X1（Terra） |
| 2 | 非 200 的 2xx 也会记录 | 属实：`claude.rs` 只判 `is_success()` | 返工 3 · X2 |
| 3 | 既有文件不保证 0600 | 属实：`append_record` 只在 `!path.exists()` 时 chmod | 返工 3 · X3 |
| 4 | 已解决按字符串排序 | 属实 | 返工 3 · X4 |
| 5 | 24h 阻塞睡眠占 blocking worker | 属实 | 返工 3 · X5 |
| 6 | a5／r3c 变异存活 | 属实（控制面 r2 变异） | 返工 3 · X6（测试敏感度，顺带补） |
