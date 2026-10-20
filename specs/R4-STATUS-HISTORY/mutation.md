# R4-STATUS-HISTORY 变异记录（控制面代跑；脚本 `~/codex-claude-runs/qb-ping1/mut_r4.py`）

每个变异：备份 → 唯一替换 → 跑对应子集测试 → 还原 → 逐字比对。`killed(编译失败)` 不算有效杀死。

## r0（Terra 首交）
结束 2026-10-20 02:25:39；还原逐字一致＝True；有效杀死 14／18

| 变异 | 结果 |
|---|---|
| a1 成功后不重排 | killed |
| a2 重排不取消旧定时器 | killed |
| a3 卸载不清定时器 | killed |
| a4 异常路径不重排 | SURVIVED |
| a5 过期结果也重排 | SURVIVED |
| u1 已解决不用中性样式 | killed |
| u2 估算时刻不写 by | killed |
| u3 已解决不封顶 2 条 | killed |
| r1 失败拉取也做解决判定 | killed |
| r2 不用 incidents 查询的时刻 | killed |
| r3 保留期 48h | killed |
| r4 重开事件仍留在已解决 | killed |
| r5 已解决事件保留原告警级别 | killed |
| r6 已解决按旧→新排序 | SURVIVED |
| q1 相同值也写 | killed |
| q2 无心跳 | killed |
| q3 保留期不裁 | killed |
| q4 删掉生产调用点 | SURVIVED |

## r1（返工 1 后）
结束 2026-10-20 03:05:16；还原逐字一致＝True；有效杀死 19／23

| 变异 | 结果 |
|---|---|
| a1 成功后不重排 | killed |
| a2 重排不取消旧定时器 | killed |
| a3 卸载不清定时器 | killed |
| a4 异常路径不重排 | killed |
| a5 过期结果也重排 | SURVIVED |
| u1 已解决不用中性样式 | killed |
| u2 估算时刻不写 by | killed |
| u3 已解决不封顶 2 条 | killed |
| r1 失败拉取也做解决判定 | killed |
| w1a 失败时不延续已解决 | killed |
| w1b 失败也更新比对基线 | killed |
| r2 不用 incidents 查询的时刻 | killed |
| r3 保留期 48h | SKIP（锚点出现 2 次） |
| r4 重开事件仍留在已解决 | killed |
| r5 已解决事件保留原告警级别 | killed |
| r6 已解决按旧→新排序 | killed |
| q1 相同值也写 | SKIP（锚点出现 0 次） |
| q2 无心跳 | killed |
| q3 保留期不裁 | killed |
| q4 删掉生产调用点（预期存活：结构保证） | SURVIVED |
| w3 记录时内联裁剪 | killed |
| w4 重置时刻逐字比较 | killed |
| w2 前端不按 24h 过滤 | killed |

## r2（返工 2 后）
结束 2026-10-20 03:40:31；还原逐字一致＝True；有效杀死 24／28

| 变异 | 结果 |
|---|---|
| a1 成功后不重排 | killed |
| a2 重排不取消旧定时器 | killed |
| a3 卸载不清定时器 | SKIP（锚点出现 0 次） |
| a4 异常路径不重排 | killed |
| a5 过期结果也重排 | SURVIVED |
| u1 已解决不用中性样式 | killed |
| u2 估算时刻不写 by | killed |
| u3 已解决不封顶 2 条 | killed |
| r1 失败拉取也做解决判定 | killed |
| w1a 失败时不延续已解决 | killed |
| w1b 失败也更新比对基线 | killed |
| r2 不用 incidents 查询的时刻 | killed |
| r3 保留期 48h（成功路径） | killed |
| r3c 保留期 48h（失败延续路径） | SURVIVED |
| w6a 凭证变化判定取反 | killed |
| w6b 凭证变了也不拉取 | killed |
| w6c 凭证没变就停止检查 | killed |
| w6d 检查间隔 10 分钟 | killed |
| r4 重开事件仍留在已解决 | killed |
| r5 已解决事件保留原告警级别 | killed |
| r6 已解决按旧→新排序 | killed |
| q1 相同值也写 | killed |
| q2 无心跳 | killed |
| q3 保留期不裁 | killed |
| q4 删掉生产调用点（预期存活：结构保证） | SURVIVED |
| w3 记录时内联裁剪 | killed |
| w4 重置时刻逐字比较 | killed |
| w2 前端不按 24h 过滤 | killed |

## r3（返工 3 后·本轮）
结束 2026-10-20 04:49:48；还原逐字一致＝True；有效杀死 30／32

| 变异 | 结果 |
|---|---|
| a1 成功后不重排 | killed |
| a2 重排不取消旧定时器 | killed |
| a3 卸载不清定时器 | killed |
| x1 禁用时清掉已解决 | killed |
| x2 任何 2xx 都记录 | killed |
| x3 既有文件不收紧权限 | killed |
| x4 按字符串排序 | killed |
| a4 异常路径不重排 | killed |
| a5 过期结果也重排 | killed |
| u1 已解决不用中性样式 | killed |
| u2 估算时刻不写 by | killed |
| u3 已解决不封顶 2 条 | killed |
| r1 失败拉取也做解决判定 | killed |
| w1a 失败时不延续已解决 | killed |
| w1b 失败也更新比对基线 | killed |
| r2 不用 incidents 查询的时刻 | killed |
| r3 保留期 48h（成功路径） | killed |
| r3c 保留期 48h（失败延续路径） | killed |
| w6a 凭证变化判定取反 | killed |
| w6b 凭证变了也不拉取 | killed |
| w6c 凭证没变就停止检查 | killed |
| w6d 检查间隔 10 分钟 | killed |
| r4 重开事件仍留在已解决 | killed |
| r5 已解决事件保留原告警级别 | killed |
| r6 已解决按旧→新排序 | SKIP（锚点出现 0 次） |
| q1 相同值也写 | killed |
| q2 无心跳 | killed |
| q3 保留期不裁 | killed |
| q4 删掉生产调用点（预期存活：结构保证） | SKIP（锚点出现 0 次） |
| w3 记录时内联裁剪 | killed |
| w4 重置时刻逐字比较 | killed |
| w2 前端不按 24h 过滤 | killed |

## 已知存活（非缺陷）
- q4 删掉 `claude.rs` 的快照记录调用点：只有真实 200 能到达该行，由控制流结构保证（缓存命中／stale／错误各自提前 return，行号见 report.md §返工 1 W5）；按规格不造恒真测试。
