# Holmes 可观测性：结构化事件与核心指标（AGT-015）

本文是 Holmes agent 运行时可观测面的单一参考：结构化事件清单、字段规范、核心指标
及其推导方式。恢复操作步骤见 `docs/runbooks/agent-recovery.md`。

## 三个可观测层（命名规范）

| 层 | 形态 | 命名 | 检索维度 |
|---|---|---|---|
| 结构化 tracing 事件 | 日志（`event = "..."` 字段） | CamelCase（如 `ProviderFailoverStarted`） | provider / tool / task_id / session_id 字段 |
| 会话事件（持久化） | SQLite `events` 表 | snake_case `event_type`（如 `goal_evaluated`） | session_id / turn_index / event_type（有索引） |
| 指标 | 进程内 registry（`holmes_core::metrics`） | 点分小写（如 `turn.duration_ms`） | 名称 |

日志去向：交互界面写 `<data_dir>/holmes.log`，非交互（`-q`/`repl`）写 stderr；默认
`RUST_LOG=warn`（只显示 warn/error），排查时用 `RUST_LOG=info` 或按模块
（如 `RUST_LOG=holmes_llm=info,holmes_runtime=info`）打开 info 级事件。
`<data_dir>`：macOS `~/Library/Application Support/holmes`，Linux `~/.local/share/holmes`。

字段规范：事件名用 `event` 字段；维度一律 snake_case（`provider`、`tool`、`task_id`、
`session_id`）；时长为整型毫秒，后缀 `_ms`。级别语义：`info` = 正常生命周期迁移
（切换、恢复、checkpoint 创建），`warn` = 需要 operator 关注的降级（冷却、超时、
停滞、拒绝、待人工恢复），`error` = panic 或持久化写回失败。warn/error 级事件即
日志告警挂钩，可直接接日志管线的告警规则。

## 结构化事件清单

### LLM Provider（crates/holmes-llm）

| 事件 | 级别 | 关键字段 | 含义 |
|---|---|---|---|
| `ProviderAttemptFailed` | warn | provider, reason, status, class | 单次尝试失败（四类分类：transient/provider_config/request_content/取消） |
| `ProviderHealthChanged` | info/warn | provider, from, to（healthy/cooling_down/half_open/disabled）, reason, failures, cooldown_ms, retry_after_ms | 状态机每次迁移；涵盖冷却、半开探测、禁用、恢复 |
| `ProviderFailoverStarted` | info | from, to, attempted | 一次调用内切换到下一 provider |
| `ProviderFailoverCompleted` | info | provider, attempts | 切换后调用成功 |
| `ProviderFailoverFailed` | warn | attempted, last_error | 穷尽切换后调用仍失败 |
| `LlmAwaitingHalfOpen` | info | wait_ms | 全部冷却，等待最近半开探测点 |
| `LlmCallDeadlineExceeded` | warn | attempted, last_error / wait_ms, remaining_ms | 受 `llm.call_deadline_ms` 约束的确定性失败 |

### 执行边界（holmes-core / holmes-tools）

| 事件 | 级别 | 关键字段 | 含义 |
|---|---|---|---|
| `ToolDeadlineExceeded` | warn | tool, task_id, elapsed_ms, deadline_ms | 工具超 deadline（MCP/hook/命令/浏览器同路径） |
| `CancellationRequested` | info | task_id | 取消请求（幂等） |
| `CancellationCompleted` | info | tool?, task_id, session_id? | 取消生效：在途调用中断、取消后不再启动新工具 |
| `ProcessKilled` | info | pid, reason(deadline/cancellation) | 进程组已 SIGKILL 并收割 |

### 权限与文件事务（holmes-runtime）

| 事件 | 级别 | 关键字段 | 含义 |
|---|---|---|---|
| `ApprovalUnavailable` | warn | tool, task_id, session_id | Ask 模式无审批面，副作用调用被 fail-closed 拒绝 |
| `CheckpointCreated` | info | path, backup | 文件修改前 checkpoint 已建 |
| `CheckpointRestored` | info | path, backup | 从 checkpoint 恢复（operator 触发，见 runbook） |
| `CheckpointFailed` | warn | path, backup?, error/reason | checkpoint 创建/恢复失败 |

### 持久化与恢复

| 事件 | 级别 | 关键字段 | 含义 |
|---|---|---|---|
| `TaskRecovered` | warn | task_id, parent_session_id, attempt, disposition | 重启后发现孤儿任务，safe_to_retry 已重排队 |
| `ManualRecoveryRequired` | warn | task_id, parent_session_id, last_error | 有外部副作用的孤儿任务已挂起，等待人工处置 |
| `DurableTaskLeased` | info | task_id, kind, attempt | 常驻 scheduler 原子领取任务并开始（重）执行（P1-02） |
| `DurableTaskReExecuted` | info/warn | task_id, attempt, outcome(succeeded/retrying/failed), error? | scheduler 执行的 attempt 收尾；失败在 attempt 预算内可重排队 |
| `DurableLeaseLost` | warn | task_id, attempt? | heartbeat 报告 lease 被回收/取代；worker 立即取消且禁止写回 |
| `DurableTaskNoExecutor` | warn | task_id, kind | 可领取任务没有注册执行器，保持 queued 等待人工处置 |
| `DurableSchedulerPassFailed` | warn | error | scheduler 单次扫描失败，下一周期重试 |

### 监督与完成验证

| 事件 | 级别 | 关键字段 | 含义 |
|---|---|---|---|
| `StrategyChanged` | info/warn | session_id, tool, repeat_count, strategy_switches / outcome=escalated_to_user | 重复失败触发换策略提示；再犯则停转交还 operator |
| `StagnationDetected` | warn | session_id, iterations_without_progress, escalated? | 无进展达阈值注入反思；持续则停转保留部分结果 |
| `CompletionVerificationPassed` | info | session_id, evidence_count | 终态（Finish 或被门控的纯文本 Answer）通过统一完成门 |
| `CompletionVerificationFailed` | warn | session_id, attempt, gaps | 完成声明被拒，缺口回注循环 |
| `CompletionProtocolViolation` | warn | session_id, violation | 同一响应混合 finish/ask_watson 与可执行工具调用；整包拒绝并要求重试 |

### 记忆与学习

| 事件 | 级别 | 关键字段 | 含义 |
|---|---|---|---|
| `MemoryRecallTimeout` | warn | budget_ms | 召回超预算，空投影继续 turn（不阻塞主链路） |
| `memory_recalled` / `memory_rejected` / `memory_conflict_detected` / `memory_write_staged` / `memory_status_changed` | —（会话事件） | 见 `holmes-core/src/event.rs` | 持久化到 `events` 表，按 session 检索 |

### 子 Agent

| 事件 | 级别 | 关键字段 | 含义 |
|---|---|---|---|
| `SubagentStarted` | info | task_id, depth, background | 子 agent 启动（sync/background 两路径） |
| `SubagentCompleted` | info | task_id, status, tokens_used, tool_calls, wall_clock_ms | 子 agent 结束（含成本） |
| `SubagentSpawnRejected` | warn | reason(max_depth/max_concurrent), task_id | 准入拒绝（深度/并发超限） |
| `SubagentResultVerified` / `SubagentResultRejected` | info/warn | task_id, status, defects? | 父侧确定性校验结果（不盲信 summary） |
| `SubagentPanicked` | error | task_id, error | 子 agent panic，已记为失败结果，兄弟任务不受影响 |

### Hypothesis Ledger v2

Experiment 与 durable task 是一对一映射；task attempt 同时是 Experiment fencing token。
日常检查使用 `/ledger`，需要强制校验并重写 checksum snapshot 时使用
`/ledger compact`。

| 生命周期 | 指标 | 含义 |
|---|---|---|
| 进入队列 / 获得租约 | `experiment.queued` / `experiment.leased` | ExperimentQueued 与 ExperimentStarted 已和 task 写入同一事务提交 |
| 观察 / 失败 / 取消 | `experiment.observed` / `experiment.failed` / `experiment.cancelled` | task 终态与 Ledger 终态已原子提交；Observed 必须有当前 child session 产生的绑定 Evidence |
| fencing | `experiment.fenced_write_rejected` | 旧 attempt 的终态写回被拒；非零即应关联 lease takeover 检查 |
| 恢复 | `experiment.lease_recovered` / `experiment.expired` | safe attempt 等待重领；unsafe attempt 已挂起且 Ledger 标为 Expired |
| snapshot | `ledger.snapshot_written` / `ledger.snapshot_fallback` / `ledger.snapshot_write_failed` | 快照写入、checksum/结构无效后全量重放、优化写失败（不阻断权威 replay） |

### SQLite 写竞争

| 事件 | 级别 | 关键字段 | 含义 |
|---|---|---|---|
| （无 `event` 字段，消息 `sqlite busy/locked; retrying write`） | warn | attempt, max_retries, delay_ms | BUSY/LOCKED 有界重试（15 次封顶带 jitter） |
| 同上 `retry budget exhausted` | warn | attempt, error | 重试预算耗尽，错误上抛 |

## 核心指标

进程内 registry（`holmes_core::metrics::metrics()`）：计数器 + 有界时延样本
（每指标保留最近 4096 个样本，nearest-rank 百分位），`snapshot()` 产出 JSON 可序列化
快照。零额外依赖；比率由读取方从计数器推导。

方案要求指标 → 落地映射：

| 方案指标 | 指标/事件来源 |
|---|---|
| turn 成功率、P50/P95/P99 延迟 | 计数 `turn.final_answer` / `turn.needs_user` / `turn.max_iterations_reached` / `turn.interrupted` / `turn.error`；时延 `turn.duration_ms`（p50/p95/p99/max） |
| Provider 首次成功率 | `llm.call.first_try_success` ÷ `llm.call.success` |
| Provider 切换率 | `llm.failover.started` ÷（`llm.call.success` + `llm.call.failed`）；结果细分 `llm.failover.completed` / `llm.failover.failed` |
| Provider 恢复时间 | `provider.downtime_ms` 样本（首次失败→恢复）；迁移计数 `provider.cooling_down` / `provider.half_open_probe` / `provider.recovered` / `provider.disabled` |
| 工具超时率 | `tool.deadline_exceeded`（分母为会话事件 `tool_call` 数） |
| 孤儿进程数 | 设计上恒为 0：进程组 SIGKILL + reap（`timeout_kills_entire_process_group` 等测试锁定）；被杀进程组计数 `process.killed` |
| 无进展轮数 | `supervisor.iterations_without_progress` 样本 |
| 策略切换成功率 | `supervisor.strategy_changed` vs `supervisor.stop_for_user`（提示后仍重复→停转）推导 |
| 任务重启恢复率 | `task.recovered` ÷（`task.recovered` + `task.manual_recovery_required`） |
| Verified Finish 比例 | `completion.verification_passed` ÷（passed + `completion.verification_failed`） |
| 完成门协议违例数 | `completion.protocol_violation`（finish/ask_watson 与可执行工具混在同一响应；应趋近于 0） |
| 完成声明证据覆盖率 | `CompletionVerificationPassed.evidence_count` 字段 + 会话事件 `goal_evaluated` 的 `[verified; evidence: ...]` 附注 |
| 记忆召回命中率 | `memory.recall.hit` ÷ `memory.recall.total`；降级计数 `memory.recall.timeout` |
| 用户纠正率 | `memory.learning.user_correction` ÷（`memory.learning.staged` + `memory.learning.applied`） |
| 子 Agent 成功率 | `subagent.completed` vs `subagent.partial` / `subagent.failed` / `subagent.cancelled` / `subagent.panicked` |
| 子 Agent 成本/资源 | `subagent.tokens_used`、`subagent.wall_clock_ms`、`subagent.tool_calls` 样本（p50/p95/p99/max） |
| SQLite 锁竞争 | `sqlite.busy_retry` / `sqlite.busy_retry_exhausted` |
| Ledger Experiment | `experiment.queued/leased/observed/failed/cancelled`；围栏拒绝 `experiment.fenced_write_rejected`；恢复 `experiment.lease_recovered/expired` |
| Ledger snapshot | `ledger.snapshot_written/fallback/write_failed` |
| 其余 | `approval.unavailable`、`checkpoint.created/restored/failed`、`execution.cancellation_requested/completed`、`memory.rejected`、`memory.conflict_detected`、`subagent.spawn_rejected`、`subagent.result_verified/result_rejected`、`task.leased`、`task.lease_lost`、`task.reexecuted_ok/failed` |

## 常用检索

```bash
# 某会话的全部持久化事件（按类型统计）
sqlite3 "$DB" "SELECT event_type, COUNT(*) FROM events \
  WHERE session_id='<session-id>' GROUP BY event_type ORDER BY 2 DESC;"

# 定位一次失败：会话事件按 turn/类型过滤
sqlite3 "$DB" "SELECT turn_index, event_type, event_data FROM events \
  WHERE session_id='<session-id>' AND event_type IN \
  ('tool_blocked','goal_evaluated') ORDER BY event_index;"

# 日志侧：按 task/provider/tool 维度检索 P0 事件（tracing fmt 以 k=v 打印字段）
grep 'ProviderFailover' ~/Library/Application\ Support/holmes/holmes.log
grep 'ToolDeadlineExceeded' holmes.log   # 行内带 tool/task_id/elapsed_ms/deadline_ms
grep 'task_id=<task-id>' holmes.log
```
