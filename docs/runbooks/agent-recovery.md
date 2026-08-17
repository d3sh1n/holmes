# Runbook: Holmes Agent 人工恢复（AGT-015/016，工作流 H）

本手册覆盖 Holmes 单机 agent 需要人工介入的故障场景。每一步都以当前已实现的
行为为依据；涉及 SQL 的操作请先备份数据库文件。

相关文档：事件与指标口径见 `docs/observability.md`；可靠性设计见
`.hermes/plans/2026-08-11_200617-agent-production-readiness-remediation.md`；
24h 长稳 soak 的运行方法与 SLO 阈值见 `docs/runbooks/long-soak.md`。

## 0. 状态位置速查

| 状态 | 位置 |
|---|---|
| 权威状态库（会话、事件、任务） | `<data_dir>/holmes.db`（SQLite，WAL） |
| 长期记忆库 | `<data_dir>/memory.db` |
| transcript 投影（可重建，非权威） | `<data_dir>/sessions/<session-id>/transcript.jsonl` |
| 文件修改 checkpoint | `<data_dir>/sessions/<session-id>/checkpoints/{payloads,manifests}/` |
| 大工具结果（schema v6+） | SQLite 内 `blobs`/`blob_chunks` 表（压缩 + SHA-256 校验，与事件同一事务）；v6 之前的库可能仍有 `sessions/<session-id>/tool-results/*.txt` sidecar 被旧事件指针引用 |

上述 `sessions/` 布局适用于默认库 `holmes.db`；任何其他文件名（测试、副本、并行实验库）使用独立的 `sessions-<sha256(canonical路径)前12位>/` 兄弟目录（P1-13，批次12），同目录两个数据库永不共享 transcript/offset/projector。
| 运行日志 | `<data_dir>/holmes.log`（交互界面）或 stderr（`-q`/`repl`） |
| 配置 | `<data_dir>/config.yaml`（模板：仓库根 `config.default.yaml`） |

`<data_dir>`：macOS `~/Library/Application Support/holmes`，Linux `~/.local/share/holmes`。

下文统一用 `$DB` 指代 `<data_dir>/holmes.db`：

```bash
DB="$HOME/Library/Application Support/holmes/holmes.db"   # macOS
# DB="$HOME/.local/share/holmes/holmes.db"                # Linux
cp "$DB" "$DB.bak.$(date +%s)"   # 任何手工 SQL 之前先备份
```

## 1. 进程崩溃 / kill -9 之后

重启即自动恢复，无需人工步骤：

1. SQLite 单事务提交保证没有半初始化会话；已提交事件不丢失。
2. 启动时自动执行后台任务恢复（`recover_durable_tasks`）：
   - 租约过期的 `running` 任务中，`safe_to_retry` 的自动重排队
     （`running → recovering → retrying`，记 `TaskRecovered` 事件，可再次被租约执行）；
   - 有外部副作用的挂起为 `manual_recovery_required`（记 `ManualRecoveryRequired`
     事件），**绝不自动重执行**，terminal 状态不可再被租约；
   - 已终态（succeeded/failed/cancelled）任务不动，已确认副作用不会重复。
3. 常驻 `DurableTaskScheduler`（P1-02，默认 30s 周期）在进程存活期间持续收敛：
   - **dead-owner reaper**：lease owner 进程（`pid-<pid>-<uuid>`）已确认死亡时
     立即过期其租约，不必等 300s 租约到期；到期租约同样按启动恢复的路径处置；
   - **lease & execute**：`queued`/`retrying` 任务由 scheduler 以条件 UPDATE 原子
     领取并执行（每 attempt 的 fencing token 随租约递增，heartbeat/checkpoint/
     终态写入必须匹配 owner + fencing，旧 attempt 写入一律被拒）；
   - worker 运行中 heartbeat 一旦报告 lease 丢失，立即取消并禁止其写回。
4. **结果投递**：终态结果在父会话的每个 turn（含 resume 后首个 turn）从数据库
   读取未投递任务，事件追加与 `delivered` 标记在同一事务提交 —— 崩溃于任何
   时间点都只会"未投递"，重启后补投，绝不重复呈现。
5. 启动横幅会提示：`⚠ Recovered N orphaned background task(s): X requeued, Y
   suspended for manual recovery`。看到 Y > 0 时按第 2 节处置。

恢复本身是幂等的：恢复过程中再次崩溃，下次启动重跑即可（扫描+处置在单事务内）。

### 1.1 Ledger Experiment 的附加不变量

带 `tasks.case_id + tasks.experiment_id` 的行是 Hypothesis Ledger v2 Experiment，不能按
普通后台任务理解：

- enqueue/lease/terminal 与 `ExperimentQueued/Started/Observed|Failed|Cancelled` 在同一
  SQLite 事务提交；`attempt` 就是 fencing token。
- safe-to-retry 的租约丢失保留 Experiment=Running，重领时用更大的 attempt 执行
  Running→Running takeover；旧 worker 的 heartbeat/终态写入会被拒绝。
- unsafe 的租约丢失原子进入 `manual_recovery_required + ExperimentExpired`。Expired
  Experiment **不可重新排队**；核对后应在父会话规划一个新 Experiment，而不是手改旧
  task 为 queued。
- `Observed` 只接受 `VerifiedAgentTaskResult` 中 `kind=ledger_evidence` 的引用，且引用必须
  存在、绑定当前 Experiment、来源必须是当前 child session。

交互检查：`/ledger`；完整 JSON：`/ledger json`。数据库检查：

```bash
sqlite3 "$DB" "SELECT task_id,case_id,experiment_id,state,attempt,lease_owner \
  FROM tasks WHERE experiment_id IS NOT NULL ORDER BY updated_at DESC;"
```

## 2. `manual_recovery_required` 任务处置

含义：进程死亡时该任务正在执行，且它带外部副作用（发消息、写远端、删除等），
系统无法判断副作用是否已发生，因此挂起等待人工核对。

步骤：

```bash
# 1. 列出待处置任务
sqlite3 "$DB" "SELECT task_id, parent_session_id, child_session_id, kind, \
  description, attempt, last_error, created_at FROM tasks \
  WHERE state='manual_recovery_required';"

# 2. 查看任务已记录的结果/检查点
sqlite3 "$DB" "SELECT result, checkpoint FROM tasks WHERE task_id='<task-id>';"

# 3. 重放子会话事件，确认副作用执行到哪一步（child_session_id 来自第 1 步）
sqlite3 "$DB" "SELECT event_index, event_type, substr(event_data,1,400) FROM events \
  WHERE session_id='<child-session-id>' ORDER BY event_index;"
```

然后带外核对副作用是否真实发生（查目标系统、问收件人、看远端状态）：

- **副作用未发生或不完整** → 标记失败，由人或后续会话决定重来：
  ```bash
  sqlite3 "$DB" "UPDATE tasks SET state='failed', \
    last_error='operator closed: side effect not observed', \
    updated_at=datetime('now') WHERE task_id='<task-id>';"
  ```
- **副作用已确认发生** → 标记成功，避免任何重试：
  ```bash
  sqlite3 "$DB" "UPDATE tasks SET state='succeeded', \
    result='operator verified side effect completed', \
    updated_at=datetime('now') WHERE task_id='<task-id>';"
  ```
- **确认副作用可安全重放**（幂等，或带 idempotency key）→ 才可重新排队：
  ```bash
  sqlite3 "$DB" "UPDATE tasks SET state='queued', lease_owner=NULL, \
    lease_expires_at=NULL, updated_at=datetime('now') WHERE task_id='<task-id>';"
  ```
  警告：这是唯一会让任务再次执行的路径。不确定副作用是否幂等时不要用。

上述手工重排只适用于 `experiment_id IS NULL` 的普通任务。Ledger Experiment 在 unsafe
过期后已是 Expired 终态，必须创建新 Experiment；直接改 task 会被租约校验拒绝并造成
运维噪声。

## 3. LLM Provider 故障

状态机在内存中（Healthy / CoolingDown / HalfOpen / Disabled），事件
`ProviderHealthChanged`（字段 from/to/reason/cooldown_ms）。

- **CoolingDown**（超时、连接失败、429、5xx）：自动恢复，无需操作。冷却窗口
  基数 5s 指数翻倍封顶 5min（±20% jitter），遇 `Retry-After` 优先采纳；窗口结束
  自动半开探测，成功即恢复（记 `provider.downtime_ms` 指标），失败窗口翻倍。
- **全部 Provider 冷却**：调用会等最近的半开点，但不超过
  `llm.call_deadline_ms`，超时确定性失败（`LlmCallDeadlineExceeded`）。频繁出现
  说明上游普遍异常：检查网络/配额，或调大 `llm.provider_cooldown_max_ms` 以外的
  配置前先确认不是目标站故障。
- **Disabled**（401/403/欠费/未知模型等配置错误）：该 Provider 本进程内不再被
  选中。修复 `config.yaml` 里的 api_key / model / base_url 后**重启进程**即可
  清除（状态不持久化）。用 `holmes setup` 可重走配置向导。
- 切换全轨迹：日志 grep `ProviderFailoverStarted/Completed/Failed`（默认 warn 级别
  不含 info 事件，用 `RUST_LOG=holmes_llm=info` 打开）。

## 4. 卡死的 turn / 后台任务

- **工具/MCP/Hook 挂起**：受 `execution.tool_deadline_ms`（默认 300s）与
  `execution.mcp_request_timeout_ms`（默认 30s）约束，超时记
  `ToolDeadlineExceeded` 并释放 turn；子进程整组 SIGKILL 并收割
  （`ProcessKilled`），不留孤儿。整体 turn 预算：`execution.turn_deadline_ms`
  （默认不限）。
- **operator 主动取消**：界面内 Esc/Ctrl+C → `CancellationRequested`，
  迭代边界干净退出，取消后不再启动新工具。
- **后台子 agent**：父 turn 取消会传播到子任务；重启后孤儿任务按第 1 节恢复。
  查任务输出用 `get_task_output` 工具或直接查库：
  ```bash
  sqlite3 "$DB" "SELECT task_id, state, attempt, last_error FROM tasks \
    WHERE state IN ('queued','running','retrying');"
  ```
- **重复失败 / 停滞**：监督器自动处理（换策略提示 → 停转交还 operator；
  反思提示 → 保留部分结果停转），事件 `StrategyChanged` / `StagnationDetected`，
  停转消息里带 Remaining work 清单，可直接作为下一轮输入继续。

## 5. 文件被误改：checkpoint 恢复

每次 `write_file`/`edit_file` 前自动在持久会话目录
`<data_dir>/sessions/<session-id>/checkpoints/` 写一份 checkpoint（记
`CheckpointCreated` 事件，含原路径与 checkpoint id）：

- `payloads/<checkpoint-id>.bak`：写入前的完整内容（同文件系统临时文件 +
  fsync + 原子 rename 落盘，中途崩溃不会留下截断的 payload）。
- `manifests/<checkpoint-id>.json`：manifest——checkpoint id（规范绝对路径
  的 SHA-256 前缀 + 纳秒时间戳 + UUID，同名不同目录、同秒多次写均不碰撞）、
  原始路径与规范路径、pre/post 内容 SHA-256 与大小、创建原因（工具名）、
  session id。目标文件在写入前不存在时记 tombstone manifest（`pre_existed:
  false`，无 payload）。

恢复（程序入口，精确匹配 manifest 的规范路径，不靠文件名猜测）：
`holmes_runtime::hooks::checkpoint::restore_latest_checkpoint(backup_dir,
target_path)`（记 `CheckpointRestored` 事件）：

- 普通 checkpoint：先校验 payload 的 SHA-256 与 manifest 一致（损坏则报错不动
  目标文件），再经原子写盖回原路径；
- tombstone checkpoint：写入前文件不存在，恢复即精确删除该次写入创建的文件。

手工恢复等价于：在 `manifests/` 中按 `canonical_path` 精确找到目标文件最新的
manifest（`created_at_ms` 最大），将其 `backup_file` 指向的 payload 复制回原
路径；tombstone 则删除原路径文件。保留策略：每路径最多 10 份、payload 总量
上限 64 MiB，超出自动清理最旧的。

## 6. transcript.jsonl 损坏或缺失

transcript 只是 `events` 表的投影，数据库才是权威。**`SessionDB::open` 会自动
reconcile**（P1-08）：逐个会话比对权威事件数/最大序号、`projection_state` 里持久化的
投影水位与磁盘行数，缺失/截断/不一致的 transcript 在打开数据库时经投影 worker 自动
重建，无需人工介入。

如需手工重建（与 `SessionDB::rebuild_transcript` 写回的内容一致：event_data 逐行原样
输出）：

```bash
sqlite3 "$DB" "SELECT event_data FROM events WHERE session_id='<session-id>' \
  ORDER BY event_index;" > "<data_dir>/sessions/<session-id>/transcript.jsonl"
```

## 7. SQLite 锁竞争（AGT-019）

单连接 + WAL + `busy_timeout=5000` + 有界 BUSY/LOCKED 重试（15 次封顶带 jitter，
仅重试锁错误，永久错误首次即抛）。并行子 agent 的写被连接互斥锁串行化，正确性
由 `crates/holmes-session/tests/concurrency_tests.rs` 锁定（8 并发写 + 双句柄同
文件）。

出现 `sqlite busy/locked; retrying write` 日志（指标 `sqlite.busy_retry`）说明有
竞争但未失败；`retry budget exhausted`（指标 `sqlite.busy_retry_exhausted`）才需要
介入：检查是否有第二个 holmes 进程指向同一数据目录，或外部工具（如 sqlite3 CLI
未提交的事务）持锁。按方案 §11.2，连接池/外部数据库是有意的后续决策，当前阶段
不迁移。

## 8. 升级/回滚注意

### Ledger snapshot 损坏/重建

`case_ledger_events` 永远是权威记录，`case_ledger_snapshots` 只是有 checksum 的加速投影。
load 会校验 checksum、case、projected_seq/version 和尾部 replay；任一不一致自动全量
重放并增加 `ledger.snapshot_fallback`，不会把损坏快照当作事实。

```text
/ledger compact
```

会从权威 event stream 重建当前 case 快照。若 CLI 无法启动，可先备份数据库，再删除该
case 的单行快照；下次 load 会全量 replay，达到 `ledger.snapshot_every_events` 后自动写回：

```bash
sqlite3 "$DB" "DELETE FROM case_ledger_snapshots WHERE case_id='<case-id>';"
```

- schema migration 只增不改（`schema_version` 表驱动）；回滚二进制前不要删列。
- 后台任务终态不可复活是刻意的：不要用 SQL 把 terminal 任务改回 running，除非
  走第 2 节的完整核对流程。
- 记忆/技能的生命周期迁移（staged→active→disabled→archived、版本回滚）全部有
  `memory_status_changed` 审计事件；技能 promote 需要 validation passed + 非空
  approved_by，不要用 SQL 绕过。
