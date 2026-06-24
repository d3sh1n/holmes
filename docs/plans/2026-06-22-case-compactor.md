# Holmes CaseCompactor Implementation Plan

> **For Hermes:** Use subagent-driven-development skill to implement this plan task-by-task.

**Goal:** Implement automatic evidence-preserving context compression for Holmes, verified through deterministic harness scenarios and runtime tests.

**Architecture:** Add a `CaseCompactor` runtime engine that estimates context pressure, performs static evidence-preserving compaction, records `CompressionApplied`, and is used by both automatic runtime compression and manual `/compress`. Keep phase 1 deliberately narrow: no LLM summarizer yet, only a robust static fallback that preserves system prompt, initial objective, active goal, latest messages, and tool call/result validity.

**Tech Stack:** Rust 2021, Tokio, serde/serde_yaml, existing `holmes-runtime`, `holmes-core`, `holmes-cli`, `holmes-harness`, `SessionDB`, `RuntimeYield`, and `Event::CompressionApplied`.

---

## Read First

- `docs/superpowers/specs/2026-06-21-holmes-learning-and-compression-spec.md`
- `docs/harness.md`
- `crates/holmes-runtime/src/runtime.rs`
- `crates/holmes-runtime/src/context.rs`
- `crates/holmes-core/src/config.rs`
- `crates/holmes-core/src/event.rs`
- `crates/holmes-cli/src/chat.rs`
- `crates/holmes-harness/src/runner.rs`
- `crates/holmes-harness/src/scenario.rs`

## Definition Of Done

- `holmes harness scenarios/long-compression.yaml` succeeds.
- Automatic runtime compression records `compression_applied`.
- Manual `/compress` and automatic compression use the same `CaseCompactor` code path.
- Static compaction preserves system prompt, initial objective, active goal, latest messages, and valid tool call/result groups.
- Existing answer/tool harness scenarios still pass.
- These commands pass:

```bash
cargo fmt --all -- --check
cargo test -p holmes-runtime --quiet
cargo test -p holmes-harness --quiet
cargo check -p holmes-cli
cargo test --workspace --quiet
```

## Non-Goals

- Do not add LLM-based compression in this PR.
- Do not add vector memory or semantic recall.
- Do not redesign message storage.
- Do not add skill learning, curator, or session search.

## Target Code Shape

New module:

```rust
// crates/holmes-runtime/src/compaction.rs
pub struct CaseCompactor {
    last_summary: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompressionPlan {
    pub should_compress: bool,
    pub force: bool,
    pub before_count: usize,
    pub estimated_tokens: u64,
    pub threshold_tokens: u64,
    pub protected_head: usize,
    pub protected_tail_start: usize,
}

#[derive(Debug, Clone)]
pub struct CompressionResult {
    pub before_count: usize,
    pub after_count: usize,
    pub summary: String,
    pub preserved_keys: Vec<String>,
    pub method: holmes_core::CompressionMethod,
}
```

Runtime integration point:

```rust
// crates/holmes-runtime/src/runtime.rs
loop {
    self.maybe_compact(false).await?;
    let frame = self.perception.perceive(&self.context);
    // existing deliberation/action loop
}
```

Manual CLI integration point:

```rust
// crates/holmes-cli/src/chat.rs
"compress" | "compact" => {
    match compact_chat_context(ctx).await {
        Ok(Some(result)) => println!(
            "Context compressed: {} -> {} messages.",
            result.before_count, result.after_count
        ),
        Ok(None) => println!("Context is already compact enough."),
        Err(error) => eprintln!("Error: {}", error),
    }
    SlashResult::Handled
}
```

---

### Task 1: Add Harness Compressor Override Parsing Test

**Objective:** Let harness scenarios set tiny compressor thresholds without changing global defaults.

**Files:**
- Modify: `crates/holmes-harness/src/scenario.rs`
- Test: `crates/holmes-harness/src/scenario.rs`

**Step 1: Write failing test**

Add this test module at the bottom of `crates/holmes-harness/src/scenario.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_compressor_override() {
        let scenario: HarnessScenario = serde_yaml::from_str(
            r#"
name: compression
config:
  compressor:
    enabled: true
    context_limit: 120
    threshold: 0.5
    protected_head: 1
    protected_tail_tokens: 80
    protect_last_n: 2
    target_ratio: 0.4
    max_summary_tokens: 200
    preserve_tool_groups: true
"#,
        )
        .expect("scenario parses");

        let compressor = scenario
            .config
            .expect("config override")
            .compressor
            .expect("compressor override");
        assert_eq!(compressor.context_limit, Some(120));
        assert_eq!(compressor.protect_last_n, Some(2));
        assert_eq!(compressor.enabled, Some(true));
    }
}
```

**Step 2: Run test to verify failure**

Run:

```bash
cargo test -p holmes-harness scenario::tests::parses_compressor_override --quiet
```

Expected: FAIL because `HarnessScenario` has no `config` field.

**Step 3: Write minimal implementation**

Add these structs and field:

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HarnessConfigOverride {
    #[serde(default)]
    pub compressor: Option<HarnessCompressorOverride>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HarnessCompressorOverride {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub context_limit: Option<u32>,
    #[serde(default)]
    pub threshold: Option<f64>,
    #[serde(default)]
    pub protected_head: Option<usize>,
    #[serde(default)]
    pub protected_tail_tokens: Option<u32>,
    #[serde(default)]
    pub protect_last_n: Option<usize>,
    #[serde(default)]
    pub target_ratio: Option<f64>,
    #[serde(default)]
    pub max_summary_tokens: Option<u32>,
    #[serde(default)]
    pub preserve_tool_groups: Option<bool>,
}
```

Add this field to `HarnessScenario`:

```rust
#[serde(default)]
pub config: Option<HarnessConfigOverride>,
```

Export the new types from `crates/holmes-harness/src/lib.rs`:

```rust
pub use scenario::{
    HarnessCompressorOverride, HarnessConfigOverride, HarnessExpectations, HarnessScenario,
    HarnessTool, HarnessTurn, ScriptedLlmResponse,
};
```

**Step 4: Run test to verify pass**

Run:

```bash
cargo test -p holmes-harness scenario::tests::parses_compressor_override --quiet
```

Expected: PASS.

**Step 5: Commit**

```bash
git add crates/holmes-harness/src/scenario.rs crates/holmes-harness/src/lib.rs
git commit -m "test: parse harness compressor overrides"
```

---

### Task 2: Apply Harness Compressor Overrides

**Objective:** Make `HarnessRunner` apply scenario compressor overrides to `HolmesConfig`.

**Files:**
- Modify: `crates/holmes-harness/src/runner.rs`
- Test: `crates/holmes-harness/tests/scenario.rs`

**Step 1: Write failing test**

Add this integration test:

```rust
#[tokio::test]
async fn applies_compressor_override_without_breaking_run() {
    let scenario: HarnessScenario = serde_yaml::from_str(
        r#"
name: override-smoke
config:
  compressor:
    context_limit: 120
    threshold: 0.5
turns:
  - input: hello
scripted_responses:
  - content: '<holmes_decision>{"type":"answer","message":"ok"}</holmes_decision>'
expectations:
  final_contains:
    - ok
  max_errors: 0
"#,
    )
    .expect("scenario parses");

    let report = HarnessRunner::new()
        .run(scenario)
        .await
        .expect("run scenario");
    assert!(report.success, "{:#?}", report.failed_expectations);
}
```

**Step 2: Run test to verify failure**

Run:

```bash
cargo test -p holmes-harness applies_compressor_override_without_breaking_run --quiet
```

Expected: FAIL if Task 1 has not been implemented; otherwise it may pass before overrides are applied. If it passes early, keep the test as regression coverage.

**Step 3: Write minimal implementation**

Replace `harness_config()` in `crates/holmes-harness/src/runner.rs` with an override-aware function:

```rust
fn harness_config(scenario: &HarnessScenario) -> HolmesConfig {
    let mut config = HolmesConfig::default();
    config.agent.max_iterations = 12;
    config.llm.providers.clear();

    if let Some(overrides) = scenario.config.as_ref() {
        if let Some(compressor) = overrides.compressor.as_ref() {
            if let Some(value) = compressor.enabled {
                config.compressor.enabled = value;
            }
            if let Some(value) = compressor.context_limit {
                config.compressor.context_limit = value;
            }
            if let Some(value) = compressor.threshold {
                config.compressor.threshold = value;
            }
            if let Some(value) = compressor.protected_head {
                config.compressor.protected_head = value;
            }
            if let Some(value) = compressor.protected_tail_tokens {
                config.compressor.protected_tail_tokens = value;
            }
            if let Some(value) = compressor.protect_last_n {
                config.compressor.protect_last_n = value;
            }
            if let Some(value) = compressor.target_ratio {
                config.compressor.target_ratio = value;
            }
            if let Some(value) = compressor.max_summary_tokens {
                config.compressor.max_summary_tokens = value;
            }
            if let Some(value) = compressor.preserve_tool_groups {
                config.compressor.preserve_tool_groups = value;
            }
        }
    }

    config
}
```

Update the call site:

```rust
harness_config(&scenario),
```

This depends on Task 4 adding the new `CompressorConfig` fields. If Task 4 is not done yet, temporarily apply only existing fields and return to this task after Task 4.

**Step 4: Run test to verify pass**

Run:

```bash
cargo test -p holmes-harness applies_compressor_override_without_breaking_run --quiet
```

Expected: PASS.

**Step 5: Commit**

```bash
git add crates/holmes-harness/src/runner.rs crates/holmes-harness/tests/scenario.rs
git commit -m "test: apply harness config overrides"
```

---

### Task 3: Add Failing Long Compression Scenario

**Objective:** Add a deterministic harness scenario that will fail until automatic compression emits `compression_applied`.

**Files:**
- Create: `scenarios/long-compression.yaml`
- Modify: `crates/holmes-harness/tests/scenario.rs`

**Step 1: Write failing scenario**

Create `scenarios/long-compression.yaml`:

```yaml
name: long-compression
description: Proves Holmes compresses a long case before continuing.
mode: pentest
config:
  compressor:
    enabled: true
    context_limit: 120
    threshold: 0.5
    protected_head: 1
    protected_tail_tokens: 80
    protect_last_n: 2
    target_ratio: 0.5
    max_summary_tokens: 200
    preserve_tool_groups: true
turns:
  - input: "Initial authorized objective: inspect staging.example only. Keep authorization scope."
  - input: "Observation AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
  - input: "Observation BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB"
scripted_responses:
  - content: '<holmes_decision>{"type":"answer","message":"scope recorded"}</holmes_decision>'
  - content: '<holmes_decision>{"type":"answer","message":"observation A recorded"}</holmes_decision>'
  - content: '<holmes_decision>{"type":"answer","message":"compression path completed"}</holmes_decision>'
expectations:
  final_contains:
    - compression path completed
  event_types:
    - user_message
    - compression_applied
    - turn_complete
  max_errors: 0
```

Add test:

```rust
#[tokio::test]
async fn runs_long_compression_scenario() {
    let scenario_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scenarios/long-compression.yaml");
    let scenario = HarnessScenario::from_path(scenario_path).expect("load scenario");
    let report = HarnessRunner::new()
        .run(scenario)
        .await
        .expect("run scenario");

    assert!(report.success, "{:#?}", report.failed_expectations);
    assert!(report
        .events
        .iter()
        .any(|event| event.event_type == "compression_applied"));
}
```

**Step 2: Run test to verify failure**

Run:

```bash
cargo test -p holmes-harness runs_long_compression_scenario --quiet
```

Expected: FAIL with missing event type `compression_applied`.

**Step 3: Do not implement yet**

This task is intentionally red. Leave it failing until the compactor and runtime integration tasks are done.

**Step 4: Commit**

```bash
git add scenarios/long-compression.yaml crates/holmes-harness/tests/scenario.rs
git commit -m "test: add long compression harness scenario"
```

---

### Task 4: Expand CompressorConfig Defaults

**Objective:** Add the config fields required by the spec while preserving backward compatibility.

**Files:**
- Modify: `crates/holmes-core/src/config.rs`

**Step 1: Write failing test**

Add to the existing tests or create a test module at the bottom of `config.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compressor_defaults_enable_static_compaction() {
        let config = HolmesConfig::default();

        assert!(config.compressor.enabled);
        assert_eq!(config.compressor.protect_last_n, 20);
        assert_eq!(config.compressor.target_ratio, 0.25);
        assert_eq!(config.compressor.max_summary_tokens, 12_000);
        assert!(config.compressor.preserve_tool_groups);
    }
}
```

**Step 2: Run test to verify failure**

Run:

```bash
cargo test -p holmes-core compressor_defaults_enable_static_compaction --quiet
```

Expected: FAIL because the fields do not exist.

**Step 3: Write minimal implementation**

Update `CompressorConfig`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompressorConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub context_limit: u32,
    pub threshold: f64,
    pub protected_head: usize,
    pub protected_tail_tokens: u32,
    #[serde(default = "default_protect_last_n")]
    pub protect_last_n: usize,
    #[serde(default = "default_target_ratio")]
    pub target_ratio: f64,
    #[serde(default = "default_max_summary_tokens")]
    pub max_summary_tokens: u32,
    #[serde(default = "default_true")]
    pub preserve_tool_groups: bool,
}

fn default_true() -> bool {
    true
}

fn default_protect_last_n() -> usize {
    20
}

fn default_target_ratio() -> f64 {
    0.25
}

fn default_max_summary_tokens() -> u32 {
    12_000
}
```

Update `HolmesConfig::default()`:

```rust
compressor: CompressorConfig {
    enabled: true,
    context_limit: 128000,
    threshold: 0.75,
    protected_head: 3,
    protected_tail_tokens: 4000,
    protect_last_n: 20,
    target_ratio: 0.25,
    max_summary_tokens: 12_000,
    preserve_tool_groups: true,
},
```

**Step 4: Run test to verify pass**

Run:

```bash
cargo test -p holmes-core compressor_defaults_enable_static_compaction --quiet
```

Expected: PASS.

**Step 5: Commit**

```bash
git add crates/holmes-core/src/config.rs
git commit -m "feat: expand compressor config defaults"
```

---

### Task 5: Create CaseCompactor Module Skeleton

**Objective:** Add `compaction.rs` with types and no behavior beyond defaults.

**Files:**
- Create: `crates/holmes-runtime/src/compaction.rs`
- Modify: `crates/holmes-runtime/src/lib.rs`

**Step 1: Write failing import test**

Add this test at the bottom of the new file:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_compactor_starts_without_summary() {
        let compactor = CaseCompactor::default();
        assert!(compactor.last_summary().is_none());
    }
}
```

**Step 2: Run test to verify failure**

Run:

```bash
cargo test -p holmes-runtime case_compactor_starts_without_summary --quiet
```

Expected: FAIL until the module is exported and implemented.

**Step 3: Write minimal implementation**

Create `crates/holmes-runtime/src/compaction.rs`:

```rust
use holmes_core::CompressionMethod;

#[derive(Debug, Clone, Default)]
pub struct CaseCompactor {
    last_summary: Option<String>,
}

impl CaseCompactor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn last_summary(&self) -> Option<&str> {
        self.last_summary.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompressionPlan {
    pub should_compress: bool,
    pub force: bool,
    pub before_count: usize,
    pub estimated_tokens: u64,
    pub threshold_tokens: u64,
    pub protected_head: usize,
    pub protected_tail_start: usize,
}

#[derive(Debug, Clone)]
pub struct CompressionResult {
    pub before_count: usize,
    pub after_count: usize,
    pub summary: String,
    pub preserved_keys: Vec<String>,
    pub method: CompressionMethod,
}
```

Export from `crates/holmes-runtime/src/lib.rs`:

```rust
pub mod compaction;
pub use compaction::*;
```

**Step 4: Run test to verify pass**

Run:

```bash
cargo test -p holmes-runtime case_compactor_starts_without_summary --quiet
```

Expected: PASS.

**Step 5: Commit**

```bash
git add crates/holmes-runtime/src/compaction.rs crates/holmes-runtime/src/lib.rs
git commit -m "feat: add case compactor skeleton"
```

---

### Task 6: Add Token Estimation

**Objective:** Add deterministic message token estimation for compression trigger decisions.

**Files:**
- Modify: `crates/holmes-runtime/src/compaction.rs`

**Step 1: Write failing test**

```rust
use holmes_core::Message;

#[test]
fn estimates_tokens_from_message_content() {
    let messages = vec![
        Message::system("system"),
        Message::user("12345678"),
        Message::assistant("abcdefghijkl"),
    ];

    assert_eq!(estimate_message_tokens(&messages), 7);
}
```

**Step 2: Run test to verify failure**

Run:

```bash
cargo test -p holmes-runtime estimates_tokens_from_message_content --quiet
```

Expected: FAIL because `estimate_message_tokens` does not exist.

**Step 3: Write minimal implementation**

Add:

```rust
use holmes_core::Message;

pub fn estimate_message_tokens(messages: &[Message]) -> u64 {
    let chars: usize = messages
        .iter()
        .map(|message| {
            message.content.as_deref().unwrap_or_default().chars().count()
                + message
                    .tool_calls
                    .as_ref()
                    .map(|calls| {
                        calls
                            .iter()
                            .map(|call| call.function.arguments.chars().count())
                            .sum::<usize>()
                    })
                    .unwrap_or(0)
        })
        .sum();
    ((chars + 3) / 4).max(messages.len()) as u64
}
```

**Step 4: Run test to verify pass**

Run:

```bash
cargo test -p holmes-runtime estimates_tokens_from_message_content --quiet
```

Expected: PASS.

**Step 5: Commit**

```bash
git add crates/holmes-runtime/src/compaction.rs
git commit -m "feat: estimate runtime context tokens"
```

---

### Task 7: Add Compression Planning

**Objective:** Decide when compression should run from config and current session messages.

**Files:**
- Modify: `crates/holmes-runtime/src/compaction.rs`

**Step 1: Write failing tests**

```rust
use holmes_core::config::HolmesConfig;
use holmes_core::session::RuntimeSession;
use holmes_core::SessionMode;

#[test]
fn plans_compression_when_threshold_is_crossed() {
    let mut config = HolmesConfig::default();
    config.compressor.context_limit = 20;
    config.compressor.threshold = 0.5;
    config.compressor.protected_head = 1;
    config.compressor.protect_last_n = 1;

    let mut session = RuntimeSession::new("s".into(), SessionMode::Pentest)
        .with_system_prompt("system");
    session.messages.push(Message::user("x".repeat(100)));
    session.messages.push(Message::assistant("done"));

    let plan = CaseCompactor::default().plan(&session, &config, false);

    assert!(plan.should_compress);
    assert_eq!(plan.protected_head, 1);
    assert_eq!(plan.protected_tail_start, 2);
}

#[test]
fn skips_compression_when_disabled() {
    let mut config = HolmesConfig::default();
    config.compressor.enabled = false;
    config.compressor.context_limit = 1;

    let session = RuntimeSession::new("s".into(), SessionMode::Pentest)
        .with_system_prompt(&"x".repeat(100));

    let plan = CaseCompactor::default().plan(&session, &config, false);
    assert!(!plan.should_compress);
}
```

**Step 2: Run tests to verify failure**

Run:

```bash
cargo test -p holmes-runtime plans_compression --quiet
```

Expected: FAIL because `plan` does not exist.

**Step 3: Write minimal implementation**

Add:

```rust
use holmes_core::config::HolmesConfig;
use holmes_core::session::RuntimeSession;

impl CaseCompactor {
    pub fn plan(
        &self,
        session: &RuntimeSession,
        config: &HolmesConfig,
        force: bool,
    ) -> CompressionPlan {
        let before_count = session.messages.len();
        let estimated_tokens = estimate_message_tokens(&session.messages);
        let threshold_tokens =
            (config.compressor.context_limit as f64 * config.compressor.threshold) as u64;
        let protected_head = config.compressor.protected_head.min(before_count);
        let protect_last_n = config.compressor.protect_last_n.min(before_count);
        let protected_tail_start = before_count.saturating_sub(protect_last_n);
        let has_middle = protected_head < protected_tail_start;
        let should_compress = config.compressor.enabled
            && has_middle
            && (force || estimated_tokens >= threshold_tokens.max(1));

        CompressionPlan {
            should_compress,
            force,
            before_count,
            estimated_tokens,
            threshold_tokens,
            protected_head,
            protected_tail_start,
        }
    }
}
```

**Step 4: Run tests to verify pass**

Run:

```bash
cargo test -p holmes-runtime plans_compression --quiet
```

Expected: PASS.

**Step 5: Commit**

```bash
git add crates/holmes-runtime/src/compaction.rs
git commit -m "feat: plan case compaction"
```

---

### Task 8: Implement Static Message Compaction

**Objective:** Replace middle messages with a compact assistant summary while preserving head and tail.

**Files:**
- Modify: `crates/holmes-runtime/src/compaction.rs`

**Step 1: Write failing test**

```rust
#[test]
fn static_compaction_preserves_head_summary_and_tail() {
    let mut config = HolmesConfig::default();
    config.compressor.context_limit = 20;
    config.compressor.threshold = 0.5;
    config.compressor.protected_head = 1;
    config.compressor.protect_last_n = 2;

    let mut session = RuntimeSession::new("s".into(), SessionMode::Pentest)
        .with_system_prompt("system prompt");
    session.messages.push(Message::user("initial authorized objective"));
    session.messages.push(Message::assistant("old reasoning"));
    session.messages.push(Message::user("latest question"));
    session.messages.push(Message::assistant("latest answer"));

    let mut compactor = CaseCompactor::default();
    let plan = compactor.plan(&session, &config, true);
    let result = compactor
        .compress_session(&mut session, &config, plan)
        .expect("compression result")
        .expect("compressed");

    assert_eq!(result.before_count, 5);
    assert!(result.after_count < result.before_count);
    assert_eq!(session.messages[0].content.as_deref(), Some("system prompt"));
    assert!(session
        .messages
        .iter()
        .any(|message| message.content.as_deref().unwrap_or("").contains("Case Compaction Summary")));
    assert!(session
        .messages
        .iter()
        .any(|message| message.content.as_deref() == Some("latest question")));
}
```

**Step 2: Run test to verify failure**

Run:

```bash
cargo test -p holmes-runtime static_compaction_preserves_head_summary_and_tail --quiet
```

Expected: FAIL because `compress_session` does not exist.

**Step 3: Write minimal implementation**

Add:

```rust
use anyhow::Result;
use holmes_core::tool_types::Role;
use holmes_core::Message;

impl CaseCompactor {
    pub fn compress_session(
        &mut self,
        session: &mut RuntimeSession,
        config: &HolmesConfig,
        plan: CompressionPlan,
    ) -> Result<Option<CompressionResult>> {
        if !plan.should_compress {
            return Ok(None);
        }

        let before_count = session.messages.len();
        let head_end = plan.protected_head.min(before_count);
        let tail_start = plan.protected_tail_start.min(before_count);
        if head_end >= tail_start {
            return Ok(None);
        }

        let middle = &session.messages[head_end..tail_start];
        let summary = build_static_summary(session, middle, config);
        let summary_message = Message::assistant(summary.clone());

        let mut compacted = Vec::new();
        compacted.extend_from_slice(&session.messages[..head_end]);
        compacted.push(summary_message);
        compacted.extend_from_slice(&session.messages[tail_start..]);
        sanitize_orphan_tool_messages(&mut compacted);

        session.messages = compacted;
        self.last_summary = Some(summary.clone());

        Ok(Some(CompressionResult {
            before_count,
            after_count: session.messages.len(),
            summary,
            preserved_keys: vec![
                "system_prompt".into(),
                "protected_head".into(),
                "protected_tail".into(),
            ],
            method: CompressionMethod::StaticFallback,
        }))
    }
}

fn build_static_summary(
    session: &RuntimeSession,
    middle: &[Message],
    config: &HolmesConfig,
) -> String {
    let active_goal = session
        .context
        .summary
        .trim()
        .is_empty()
        .then_some("none")
        .unwrap_or(session.context.summary.as_str());
    let middle_count = middle.len();
    let sample = middle
        .iter()
        .filter_map(|message| message.content.as_deref())
        .take(5)
        .collect::<Vec<_>>()
        .join("\n- ");

    let mut summary = format!(
        "## Case Compaction Summary\n\
         - Compacted middle messages: {middle_count}\n\
         - Active context summary: {active_goal}\n\
         - Compression target ratio: {}\n\
         \n\
         ## Preserved Evidence Notes\n\
         - {sample}",
        config.compressor.target_ratio
    );
    if summary.len() > config.compressor.max_summary_tokens as usize * 4 {
        summary.truncate(config.compressor.max_summary_tokens as usize * 4);
    }
    summary
}

fn sanitize_orphan_tool_messages(messages: &mut Vec<Message>) {
    messages.retain(|message| message.role != Role::Tool || message.tool_call_id.is_some());
}
```

`CompressionMethod` currently lives in `crates/holmes-core/src/event.rs` and is re-exported from `holmes_core`; the static variant is `CompressionMethod::StaticFallback`.

**Step 4: Run test to verify pass**

Run:

```bash
cargo test -p holmes-runtime static_compaction_preserves_head_summary_and_tail --quiet
```

Expected: PASS.

**Step 5: Commit**

```bash
git add crates/holmes-runtime/src/compaction.rs
git commit -m "feat: add static case compaction"
```

---

### Task 9: Record CompressionApplied From Runtime

**Objective:** Add a reusable runtime method that compacts context and records `Event::CompressionApplied`.

**Files:**
- Modify: `crates/holmes-runtime/src/runtime.rs`

**Step 1: Write failing runtime test**

Add to `runtime.rs` tests:

```rust
#[tokio::test]
async fn manual_runtime_compaction_records_event() {
    let llm = Arc::new(QueueLlmBackend::new(vec![]));
    let mut config = HolmesConfig::default();
    config.compressor.context_limit = 20;
    config.compressor.threshold = 0.5;
    config.compressor.protected_head = 1;
    config.compressor.protect_last_n = 1;

    let context = make_context_with_config(llm, ToolRegistry::new(), GuardChain::new(), config)
        .await;
    let mut runtime = AgentRuntime::new(context);
    runtime
        .context_mut()
        .session
        .messages
        .push(Message::user("x".repeat(100)));
    runtime
        .context_mut()
        .session
        .messages
        .push(Message::assistant("tail"));

    let result = runtime.compact_now().await.expect("compact now");
    assert!(result.is_some());

    let events = runtime
        .context()
        .session_db
        .get_events(&runtime.context().session_id)
        .await
        .expect("events");
    assert!(events
        .iter()
        .any(|stored| matches!(stored.event, Event::CompressionApplied { .. })));
}
```

`make_context_with_config` already exists in the runtime test helper area near the bottom of `crates/holmes-runtime/src/runtime.rs`.

**Step 2: Run test to verify failure**

Run:

```bash
cargo test -p holmes-runtime manual_runtime_compaction_records_event --quiet
```

Expected: FAIL because `compact_now` does not exist.

**Step 3: Write minimal implementation**

Modify `AgentRuntime`:

```rust
use crate::compaction::{CaseCompactor, CompressionResult};

pub struct AgentRuntime {
    context: RuntimeContext,
    perception: PerceptionEngine,
    deliberation: DeliberationEngine,
    action: ActionEngine,
    evidence: EvidenceEngine,
    memory: MemoryEngine,
    reflection: ReflectionEngine,
    dialogue: DialogueEngine,
    compactor: CaseCompactor,
}
```

Initialize it:

```rust
compactor: CaseCompactor::new(),
```

Add methods:

```rust
pub async fn compact_now(&mut self) -> Result<Option<CompressionResult>, RuntimeError> {
    self.compact_with_force(true).await
}

async fn maybe_compact(&mut self) -> Result<Option<CompressionResult>, RuntimeError> {
    self.compact_with_force(false).await
}

async fn compact_with_force(
    &mut self,
    force: bool,
) -> Result<Option<CompressionResult>, RuntimeError> {
    let plan = self
        .compactor
        .plan(&self.context.session, &self.context.config, force);
    let Some(result) = self
        .compactor
        .compress_session(&mut self.context.session, &self.context.config, plan)
        .map_err(|error| RuntimeError::recoverable(error.to_string()))?
    else {
        return Ok(None);
    };

    let event = Event::CompressionApplied {
        before_count: result.before_count,
        after_count: result.after_count,
        summary: result.summary.clone(),
        preserved_keys: result.preserved_keys.clone(),
        method: result.method.clone(),
    };
    append_and_ingest(&mut self.context, event).await?;
    Ok(Some(result))
}
```

**Step 4: Run test to verify pass**

Run:

```bash
cargo test -p holmes-runtime manual_runtime_compaction_records_event --quiet
```

Expected: PASS.

**Step 5: Commit**

```bash
git add crates/holmes-runtime/src/runtime.rs
git commit -m "feat: record runtime compaction events"
```

---

### Task 10: Trigger Automatic Compaction Before Deliberation

**Objective:** Run compaction before building the perception frame when the threshold is crossed.

**Files:**
- Modify: `crates/holmes-runtime/src/runtime.rs`

**Step 1: Write failing test**

Add:

```rust
#[tokio::test]
async fn run_turn_auto_compacts_before_deliberation() {
    let llm = Arc::new(QueueLlmBackend::new(vec![Ok(final_response("done"))]));
    let mut config = HolmesConfig::default();
    config.compressor.context_limit = 20;
    config.compressor.threshold = 0.5;
    config.compressor.protected_head = 1;
    config.compressor.protect_last_n = 1;

    let context = make_context_with_config(llm, ToolRegistry::new(), GuardChain::new(), config)
        .await;
    let mut runtime = AgentRuntime::new(context);
    runtime
        .context_mut()
        .session
        .messages
        .push(Message::user("x".repeat(100)));
    runtime
        .context_mut()
        .session
        .messages
        .push(Message::assistant("old tail"));

    let mut sink = VecSink::new();
    runtime.run_turn("continue", &mut sink).await.expect("turn");

    let events = runtime
        .context()
        .session_db
        .get_events(&runtime.context().session_id)
        .await
        .expect("events");
    assert!(events
        .iter()
        .any(|stored| matches!(stored.event, Event::CompressionApplied { .. })));
}
```

Add `make_context_with_config` in the test helper area if needed:

```rust
async fn make_context_with_config(
    llm: Arc<dyn LlmBackend>,
    tools: ToolRegistry,
    guards: GuardChain,
    config: HolmesConfig,
) -> RuntimeContext {
    make_context_inner(llm, tools, guards, config).await
}
```

Use the actual helper structure already present near the bottom of `runtime.rs`.

**Step 2: Run test to verify failure**

Run:

```bash
cargo test -p holmes-runtime run_turn_auto_compacts_before_deliberation --quiet
```

Expected: FAIL because `run_turn` does not call `maybe_compact`.

**Step 3: Write minimal implementation**

Insert this before `let frame = self.perception.perceive(&self.context);`:

```rust
if let Err(error) = self.maybe_compact().await {
    return self.stop_for_error(error, iterations, sink);
}
```

Do not emit `RuntimeYield` for automatic compaction in this task. The event is enough for auditability and avoids noisy CLI output.

**Step 4: Run test to verify pass**

Run:

```bash
cargo test -p holmes-runtime run_turn_auto_compacts_before_deliberation --quiet
```

Expected: PASS.

**Step 5: Commit**

```bash
git add crates/holmes-runtime/src/runtime.rs
git commit -m "feat: auto compact before deliberation"
```

---

### Task 11: Make Long Compression Harness Pass

**Objective:** Verify the harness scenario from Task 3 now passes.

**Files:**
- No new files

**Step 1: Run the failing harness test**

Run:

```bash
cargo test -p holmes-harness runs_long_compression_scenario --quiet
```

Expected: PASS after Tasks 4-10.

**Step 2: Run the CLI harness scenario**

Run:

```bash
cargo run -p holmes-cli --quiet -- harness scenarios/long-compression.yaml
```

Expected:

```json
{
  "name": "long-compression",
  "success": true
}
```

The full JSON will include more fields; only `name` and `success` need to match.

**Step 3: Commit**

```bash
git add crates/holmes-core/src/config.rs crates/holmes-runtime/src crates/holmes-harness scenarios/long-compression.yaml
git commit -m "test: verify long compression harness"
```

---

### Task 12: Route `/compress` Through CaseCompactor

**Objective:** Replace `ctx.mind_palace.compress()` with the same compaction path used by runtime auto-compression.

**Files:**
- Modify: `crates/holmes-cli/src/chat.rs`
- Test: `crates/holmes-cli/tests/slash_commands.rs`

**Step 1: Write failing test**

Add a slash-command test that asserts `/compress` still resolves and document the integration gap with a TODO if full REPL context construction is too heavy. Prefer an integration test if helpers already exist:

```rust
#[test]
fn compress_command_is_registered() {
    let registry = CommandRegistry::default();
    assert_eq!(registry.resolve("compress"), Some("compress"));
    assert_eq!(registry.resolve("compact"), Some("compress"));
}
```

If this already exists, add a focused unit test for the new helper once created:

```rust
#[tokio::test]
async fn compact_chat_context_uses_runtime_compactor() {
    // Build a minimal ChatContext with tiny compressor config and enough messages.
    // Assert result.is_some() and SessionDB contains CompressionApplied.
}
```

**Step 2: Run test to verify failure**

Run:

```bash
cargo test -p holmes-cli compact_chat_context_uses_runtime_compactor --quiet
```

Expected: FAIL until the helper exists. If only registry coverage is feasible, record this as a residual test gap in the PR.

**Step 3: Write minimal implementation**

Add helper in `chat.rs` near `run_runtime_input`:

```rust
async fn compact_chat_context(
    ctx: &mut ChatContext,
) -> anyhow::Result<Option<holmes_runtime::CompressionResult>> {
    let mode = ctx.runtime_session.mode.clone();
    let placeholder_session = RuntimeSession::new(ctx.session_id.clone(), mode.clone());
    let session = std::mem::replace(&mut ctx.runtime_session, placeholder_session);
    let placeholder_palace = MindPalace::new(ctx.session_db.clone(), ctx.memory_store.clone());
    let mind_palace = std::mem::replace(&mut ctx.mind_palace, placeholder_palace);
    let placeholder_guards = GuardChain::from_config(&ctx.config.guards);
    let runtime_guards = std::mem::replace(&mut ctx.runtime_guards, placeholder_guards);
    let placeholder_state = RuntimeState::new(mode);
    let runtime_state = std::mem::replace(&mut ctx.runtime_state, placeholder_state);
    let llm: Arc<dyn LlmBackend> = ctx.llm.clone();

    let runtime_context = RuntimeContext::new(
        session,
        ctx.session_db.clone(),
        ctx.memory_store.clone(),
        mind_palace,
        llm,
        ctx.registry.clone(),
        runtime_guards,
        runtime_state,
        ctx.config.clone(),
    );
    let mut runtime = AgentRuntime::new(runtime_context);
    let result = runtime.compact_now().await?;
    let runtime_context = runtime.into_context();

    ctx.session_id = runtime_context.session_id.clone();
    ctx.runtime_session = runtime_context.session;
    ctx.mind_palace = runtime_context.mind_palace;
    ctx.runtime_guards = runtime_context.guards;
    ctx.runtime_state = runtime_context.state;

    Ok(result)
}
```

Update the slash branch:

```rust
"compress" | "compact" => {
    match compact_chat_context(ctx).await {
        Ok(Some(result)) => println!(
            "Context compressed: {} -> {} messages.",
            result.before_count, result.after_count
        ),
        Ok(None) => println!("Context is already compact enough."),
        Err(error) => eprintln!("Error: {}", error),
    }
    SlashResult::Handled
}
```

**Step 4: Run test to verify pass**

Run:

```bash
cargo test -p holmes-cli compact_chat_context_uses_runtime_compactor --quiet
```

Expected: PASS if the helper test was added.

**Step 5: Commit**

```bash
git add crates/holmes-cli/src/chat.rs crates/holmes-cli/tests/slash_commands.rs
git commit -m "feat: route manual compression through runtime compactor"
```

---

### Task 13: Add Compression Event Type Coverage To Harness

**Objective:** Ensure harness event type extraction recognizes `compression_applied` through the existing serde tag path.

**Files:**
- Modify: `crates/holmes-harness/tests/scenario.rs`

**Step 1: Add regression assertion**

Extend `runs_long_compression_scenario`:

```rust
let compression_event = report
    .events
    .iter()
    .find(|event| event.event_type == "compression_applied")
    .expect("compression event");

assert!(serde_json::to_value(&compression_event.event)
    .expect("event json")
    .get("type")
    .and_then(|value| value.as_str())
    .is_some_and(|kind| kind == "compression_applied"));
```

**Step 2: Run test**

Run:

```bash
cargo test -p holmes-harness runs_long_compression_scenario --quiet
```

Expected: PASS.

**Step 3: Commit**

```bash
git add crates/holmes-harness/tests/scenario.rs
git commit -m "test: assert compression event reporting"
```

---

### Task 14: Full Verification

**Objective:** Run the full validation suite and fix only scoped issues.

**Files:**
- No planned edits

**Step 1: Format check**

Run:

```bash
cargo fmt --all -- --check
```

Expected: PASS.

**Step 2: Runtime tests**

Run:

```bash
cargo test -p holmes-runtime --quiet
```

Expected: PASS.

**Step 3: Harness tests**

Run:

```bash
cargo test -p holmes-harness --quiet
```

Expected: PASS.

**Step 4: CLI check**

Run:

```bash
cargo check -p holmes-cli
```

Expected: PASS.

**Step 5: Workspace tests**

Run:

```bash
cargo test --workspace --quiet
```

Expected: PASS.

**Step 6: Manual harness run**

Run:

```bash
cargo run -p holmes-cli --quiet -- harness scenarios/long-compression.yaml
```

Expected: JSON report with `"success": true` and one `compression_applied` event.

**Step 7: Commit**

```bash
git add .
git commit -m "feat: add evidence-preserving case compaction"
```

---

## Review Checklist

- [ ] Compaction is disabled when `compressor.enabled = false`.
- [ ] `CompressionApplied` is recorded exactly when messages are changed.
- [ ] Manual `/compress` does not call `MindPalace::compress()` directly.
- [ ] The static summary contains enough information to understand what was compacted.
- [ ] The system prompt remains first.
- [ ] The latest user turn remains present.
- [ ] No orphaned tool result messages remain.
- [ ] Harness answer/tool scenarios still pass.
- [ ] Long compression harness scenario passes.
- [ ] No unrelated refactors are included.

## Execution Handoff

Plan complete and saved. Ready to execute using subagent-driven-development: dispatch a fresh subagent per task with two-stage review, first spec compliance, then code quality. Proceed only after both reviews approve each task.
