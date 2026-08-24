# Authorized bounty research workflow

Holmes can attach a **case-level** bug-bounty / VDP program to a session and keep
scope, in-scope assets, ledger-verified findings, and a bounty-ready report in
sync across resume. This is a research workflow, not an exploit capability.

## Pieces

| Layer | Role |
| --- | --- |
| `holmes-core::bounty` | Program scope, asset inventory, report renderer, fail-closed gates |
| `set_program_scope` / `get_program_scope` | Persist the authorized program on the case |
| `record_asset` / `list_assets` / `report_recon` | In-scope inventory with provenance |
| `report_finding` | Finding must cite a verified Ledger Resolution ID |
| `generate_bounty_report` | Markdown from in-scope verified findings only |
| `ScopeGuard` | When a program is active, driven from it (same heuristic as `guards.scope`) |
| `BountyPreGuard` | Blocks out-of-scope assets and unattested findings |
| `BountyPostGuard` | Writes program/assets into the AttackState **free zone** |
| `BountyWorkflowMiddleware` | Materializes get/list/report from case state |
| `SkepticGate` | Still the sole writer of the validated findings zone |

Events `ProgramScopeSet` and `AssetRecorded` are replayed on resume.

## Heuristic scope (unchanged matching)

Host matching is exact host, domain suffix (`example.com` matches `api.example.com`),
and IPv4 CIDR. URL prefixes are an additional constraint for `http_request`,
`web_fetch`, and `browser` navigate. Private/loopback/metadata addresses stay
blocked unless `allow_private`.

This is **not** a hard security boundary: redirects, DNS changes, unknown/MCP
tools, and dynamically constructed commands can bypass it. The operator remains
responsible for staying inside the authorized program.

## Findings

While a program is active, Runtime + `BountyPreGuard` require:

1. At least one Ledger Resolution ID whose status is confirmed or rejected
2. Runtime `_ledger_validation` attestation (same private field as before)
3. An in-scope `affected_asset` or `location`

Unverified claims cannot become findings. Evidence fields are artifacts already
collected — never payloads.

## Reports

`generate_bounty_report` renders summary, program scope, affected assets, impact
(business/security, not an exploit writeup), high-level observation + evidence IDs,
high-level remediation, and an evidence appendix. Pass `allow_no_findings=true`
when the operator asks for an explicit empty report.
