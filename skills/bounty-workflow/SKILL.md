---
name: bounty-workflow
description: >
  Authorized bug-bounty / VDP RESEARCH workflow. Use when the operator names an
  authorized program and wants scoped recon, in-scope asset inventory, ledger-verified
  findings, and a bounty-ready markdown report.
  Do NOT use for unauthorized testing, exploit development, fuzzing playbooks,
  reverse-engineering/pwn procedures, or any request to break into a system.
license: MIT
compatibility: "Holmes agent with Hypothesis Ledger v2, GuardChain, and builtin bounty tools."
metadata:
  author: "Holmes"
  version: "1.0.0"
  category: "security-research"
  tags: ["bug-bounty", "vdp", "authorized-testing", "research-workflow"]
---

# Authorized bounty research workflow

You are operating a **research** workflow for a program the operator has already
authorized. This skill does not add exploit capability. Safety layers
(`PermissionPolicy`, `GuardChain`, `dangerous_command`, `SkepticGate`) stay in force.

## When to use

- The operator names a bug-bounty or VDP program and in-scope assets.
- The operator asks to inventory in-scope assets, file a finding against a
  **verified** Hypothesis Ledger Resolution, or generate a bounty-ready report.

Do **not** use this skill to probe hosts that are not in the active program, to
produce payloads, or to write step-by-step attack reproduction.

## Start a case

1. Call `set_program_scope` with the program name, in-scope entries (hosts,
   domain suffixes, URL prefixes, CIDRs), out-of-scope entries, and policy notes.
2. Confirm with `get_program_scope`. Resume reloads the program from the event log.
3. While a program is active, `http_request` / `web_fetch` / `browser` navigate and
   hostnames extracted from `execute_command` / `execute_python` fail closed out of
   scope using the same **heuristic** as `guards.scope` (exact host, domain suffix,
   IPv4 CIDR, plus URL prefixes). Redirects, DNS changes, unknown/MCP tools, and
   dynamically constructed commands can bypass it — you still must stay in scope.

## Gates

- **Authorized programs only.** Never set or expand scope for unauthorized testing.
- **Assets:** `record_asset` / `report_recon` assets must be in scope. Out-of-scope
  identifiers are refused. Record provenance (`how_found`), not attack steps.
- **Findings:** `report_finding` requires a Hypothesis Ledger Resolution ID that is
  actually **verified** (confirmed or rejected) and an in-scope `affected_asset`.
  Unverified claims cannot become findings. Evidence is artifacts already collected
  (ledger evidence IDs, screenshot paths, request/response hashes, log excerpts).
- **Reports:** `generate_bounty_report` includes only in-scope, ledger-verified
  findings. Impact is business/security impact, not an exploit writeup. Reproduction
  is high-level (what was observed + evidence IDs). Set `allow_no_findings=true`
  only when the operator asks for an explicit empty report.
- **SkepticGate** remains the sole writer of the validated findings zone.

## Banned patterns

- Unauthorized testing or targeting assets outside the active program
- Exploits, exploit PoCs, payloads, fuzzing/attack playbooks, weaponized reproduction
- Reverse-engineering / pwn procedures, or anything that teaches breaking into a system
- Filing a finding without a verified Ledger Resolution ID
- Putting payloads, exploit recipes, or step-by-step attack instructions in reports
- Weakening PermissionPolicy, GuardChain, or dangerous_command

## Tools

| Tool | Role |
| --- | --- |
| `set_program_scope` / `get_program_scope` | Case-level authorized program |
| `record_asset` / `list_assets` | In-scope inventory with provenance |
| `report_recon` | Recon summary; optional in-scope `assets` |
| `report_finding` | Finding after a verified Resolution |
| `generate_bounty_report` | Bounty-ready markdown |

Operator slash command: `/bounty` inspects the attached program and asset list.
