# Runbook: 24 小时长稳 soak 与 SLO（P2-02）

本手册定义「多会话、多子 agent、网络抖动」长稳验证的运行方法与发布阈值。
GitHub-hosted nightly 跑不了真 24 小时，所以分两层：

1. **nightly 近似**：`.github/workflows/nightly-reliability.yml` 的
   reliability-soak 把确定性故障注入套件循环 10 遍（harness 场景、provider
   failover、SQLite durability/crash-point matrix、durable task 恢复矩阵）。
   任何一轮失败都阻断发布，直到分诊完毕。
2. **人工 24h soak**：发布候选前按本手册在真实机器上跑一次。这是「高可用」
   声明的必要条件，不可跳过。

## 1. 运行方法

### 1.1 确定性套件长循环（必做，无需 API key）

```bash
# 建议 24h；至少覆盖一次 nightly 全量 + 8h 以上循环。
while true; do
  date
  cargo test -p holmes-harness || break
  cargo test -p holmes-llm || break
  cargo test -p holmes-session || break
  cargo test -p holmes-runtime --test durable_recovery_matrix || break
  cargo test -p holmes-runtime scheduler || break
done 2>&1 | tee soak-deterministic.log
```

任一套件失败即停止并分诊：失败即缺陷，不接受「重跑过了就算过」。

### 1.2 真实会话 soak（有 API key 时做）

用真实配置跑多会话 + 并发子 agent 工作负载，期间注入网络抖动
（断网 30s / 限速 / DNS 失败）与至少 3 次 `kill -9`：

```bash
# 示例循环：多会话交替、每会话派发子 agent 任务。
for i in $(seq 1 200); do
  holmes -q "session $i: enumerate example.com and spawn a subagent to verify" \
    || echo "turn failed at iter $i" | tee -a soak-live.log
  # 每 20 轮模拟一次进程崩溃：
  if (( i % 20 == 0 )); then pkill -9 -f 'holmes -q' || true; fi
  sleep 5
done
```

崩溃后的恢复验证无需人工 SQL，按 `agent-recovery.md` §1 确认：
重排队/挂起计数出现在启动横幅，终态结果不重复呈现。

## 2. SLO 指标与发布阈值

指标口径见 `docs/observability.md`「核心指标」表（进程内
`holmes_core::metrics::metrics()` 快照 + SQLite 事件 + 日志检索）。
soak 开始与结束各取一次 `snapshot()`，并按下表核对：

| 指标 | 数据来源 | 发布阈值 |
|---|---|---|
| 任务成功率 | `subagent.completed` ÷（completed+partial+failed+cancelled+panicked） | ≥ 95%（排除注入故障窗口内的样本后） |
| 任务重启恢复率 | `task.recovered` ÷（`task.recovered` + `task.manual_recovery_required`） | kill -9 注入后 safe_to_retry 任务 100% 收敛；manual 挂起 0 误重执行 |
| 恢复时间 | 进程重启 → 启动横幅恢复提示出现 | p100 ≤ 60s（含 migration 与 reconcile） |
| 取消延迟 | `turn.duration_ms` 中 interrupted turn 的样本；事件 `CancellationRequested` → turn 收尾 | p99 ≤ deadline + 2s 清理宽限（`CLEANUP_GRACE`） |
| 孤儿资源 | `ps` 查残留子进程；`tasks` 表 `state='running'` 且 lease 过期 > 10min 的行数 | 恒 0（进程组 SIGKILL + reap；过期租约由 reaper 收敛） |
| 投影积压 | `projection_state.projected_event_index` 水位 vs `events` 表 MAX(event_index)；`sqlite3 "$DB" "SELECT p.session_id, p.projected_event_index, (SELECT MAX(event_index) FROM events WHERE session_id=p.session_id) FROM projection_state p;"` | 稳态差值 = 0；open reconcile 后必须归零 |
| 内存趋势 | soak 首尾及每小时记录 RSS（`ps -o rss=`） | 无持续单调增长；24h 末 RSS ≤ 首小时稳定值 × 1.5 |
| SQLite 锁竞争 | `sqlite.busy_retry_exhausted` | 恒 0（`busy_retry` 非零可接受） |
| Provider 恢复 | `provider.downtime_ms` 样本、`provider.recovered` 计数 | 网络抖动结束后自动恢复，无人工干预 |

## 3. 分诊指引

- **任务成功率不达标**：按 `task_id` grep 日志（`observability.md`「常用检索」），
  区分是注入故障窗口内的预期失败还是真缺陷。
- **投影积压不归零**：检查 `reconcile` 日志与 `projection_state` 表；按
  `agent-recovery.md` §6 手工重建等价 SQL 验证一致性。
- **内存单调增长**：用 `SOAK` 期间的 RSS 序列定位拐点，优先怀疑投影队列
  （容量 1024 有界，若仍增长说明 worker 停滞）与浏览器子进程泄漏。
- **恢复时间超标**：检查 migration 重试日志（BUSY/LOCKED 退避 ≤100 次）与
  reconcile 会话数量。

soak 通过后，把 `soak-deterministic.log` / `soak-live.log` 与指标快照归档到
发布记录；不通过则按审查文档 §10 验收矩阵回退「高可用」声明。
