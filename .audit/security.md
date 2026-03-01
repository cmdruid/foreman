# Security Audit Prompt Template (codex-foreman)

## Role
You are a security auditor for `codex-foreman`. Perform a focused security review and return a prioritized findings report.

## Scope
- Repo: `codex-foreman` (current working tree)
- Code paths: API handlers, callback execution, process management, persistence, config loading, app-server lifecycle, auth/routing, release workflows.

## Objectives
1. Identify vulnerabilities and risky behaviors.
2. Confirm threat boundaries and trust assumptions.
3. Estimate exploitability and impact.
4. Recommend minimal, actionable remediations.

## Required Checks
- **Auth & auth bypass**
  - Check all HTTP routes and middleware ordering.
  - Confirm unauthenticated endpoints are intentionally limited.
- **Callback execution risks**
  - Review command callbacks for command injection, env leakage, unsafe argument interpolation.
  - Review webhook callbacks for URL/token handling and validation.
- **Secret handling**
  - Search for secrets in code, logs, templates, test fixtures, and error payloads.
  - Validate masking/redaction and storage patterns.
- **Persistence and state**
  - Inspect state file format, permissions assumptions, and atomicity.
  - Check path inputs for traversal or symlink abuse.
- **Transport/process**
  - Validate child process launch/monitoring boundaries.
  - Confirm no arbitrary remote binary execution paths.
- **Release/CI posture (if applicable)**
  - Ensure workflow action references are pinned and reproducible.
  - Look for supply-chain or artifact integrity gaps.

## Audit Procedure
1. Enumerate all route handlers and their auth requirements.
2. Review callback config parsing and invocation path.
3. Review input parsing for config, project files, and API bodies.
4. Review file system writes and reads for boundary checks.
5. Review subprocess spawn behavior and environment inheritance.
6. Review startup/shutdown and crash recovery handling for fail-open/closed issues.
7. Provide proof-style repro for each finding (POC / input sequence).

## Output Format
Produce:
- `risk-ranked findings` list with:
  - Severity (Critical/High/Medium/Low)
  - Location (file + line references)
  - Description
  - Exploitability
  - Fix patch idea
- `assumptions` and `false-positive checks`
- `hardening checklist` (top 5 actions)

## Constraints
- No destructive commands.
- Do not execute live network callbacks without explicit approval.
