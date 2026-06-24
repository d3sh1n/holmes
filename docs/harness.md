# Holmes Harness

Holmes Harness is the agent OS testbench for Holmes. It runs real
`AgentRuntime` inside a deterministic world so cognitive behavior can be tested
without calling a real model, touching the network, or depending on live tools.

```text
Scenario Layer      YAML case, turns, config, expectations
World Layer         scripted LLM, mock tools, artifact-backed outputs
Runtime Layer       real AgentRuntime
Observer Layer      RuntimeYield stream, SessionDB events, turn outcomes
Evaluator Layer     deterministic expectations and CI exit status
```

The harness should fake the outside world, not Holmes itself.

## Run

```bash
cargo run -p holmes-cli -- harness scenarios/basic-answer.yaml
cargo run -p holmes-cli -- harness scenarios/basic-tool.yaml
cargo run -p holmes-cli -- harness scenarios/interactive-ask-watson.yaml
cargo run -p holmes-cli -- harness scenarios/artifact-tool.yaml
```

The command exits with a non-zero status when expectations fail, so it can be
used in CI.

## Scenario Shape

```yaml
name: basic-tool
mode: pentest
turns:
  - input: inspect example.test
    expect_needs_user: false
tools:
  - name: echo_probe
    output: example.test is reachable
    read_only: true
artifacts:
  - path: fixtures/http/login-response.json
    as_tool_output: echo_probe
scripted_responses:
  - content: '<holmes_decision>{"type":"use_tools","calls":[...]}</holmes_decision>'
  - content: '<holmes_decision>{"type":"answer","message":"done"}</holmes_decision>'
expectations:
  final_contains:
    - done
  event_types:
    - tool_call
    - tool_result
  event_sequence:
    - user_message
    - tool_call
    - tool_result
    - turn_complete
  yield_types:
    - tool_started
    - tool_finished
  tool_calls:
    - echo_probe
  needs_user_count: 0
  max_errors: 0
```

Interactive fixtures can model Holmes pausing for Watson and then continuing:

```yaml
turns:
  - input: test the login flow
    expect_needs_user: true
    reply: yes, authorized for staging only
scripted_responses:
  - content: '<holmes_decision>{"type":"ask_watson","question":"Authorized?","options":["yes","no"]}</holmes_decision>'
  - content: '<holmes_decision>{"type":"answer","message":"Authorization recorded"}</holmes_decision>'
expectations:
  needs_user_count: 1
  yield_types:
    - needs_user_input
    - final_answer
```

## Report

The harness prints JSON with:

- `success`: overall expectation status.
- `turns`: each user turn and runtime outcome.
- `metrics`: answer/tool/error counts.
- `failed_expectations`: machine-readable failure reasons.
- `yields`: streamed runtime output such as tool start/finish and final answers.
- `events`: persisted Holmes events with storage metadata and nested event data.

## Current Scope

This first harness layer proves that Holmes can:

- run deterministic agent turns through `AgentRuntime`;
- model real-time Watson interaction with inline replies;
- capture `RuntimeYield` output;
- persist and report event history;
- mock tool execution with literal or artifact-backed outputs;
- apply scenario-level config overrides for compression and learning;
- fail CI on missing answers, missing event types, missing event sequences,
  missing yield types, missing tool calls, wrong `NeedsUser` counts, or excess
  errors.

## Next Extensions

- Add replay scenarios sourced from real Holmes sessions.
- Add judge-based scoring for open-ended results.
- Add budget/latency metrics.
- Add payload matchers for event fields and yielded content.
- Add learning/compression/deduction/skill/curator/session-search regression suites.

## Deduction Suite

The harness can now validate Holmes' cognitive kernel as a deduction ledger:

```text
tool_result
  -> evidence_observed
  -> fact_recorded
  -> hypothesis_proposed
  -> prediction_made
  -> experiment_planned
  -> hypothesis_supported
  -> hypothesis_confirmed
  -> conclusion_drawn
```

Run:

```bash
cargo run -p holmes-cli -- harness scenarios/deductive-login-enumeration.yaml
cargo run -p holmes-cli -- harness scenarios/deductive-login-no-enumeration.yaml
cargo run -p holmes-cli -- harness scenarios/deductive-llm-trace.yaml
```

This verifies that Holmes does not merely answer correctly; it records why the
answer follows from observed evidence. The `deductive-llm-trace` scenario also
proves that model decisions can explicitly emit a structured `deduce` directive
and have the runtime persist it as first-class deduction events.
