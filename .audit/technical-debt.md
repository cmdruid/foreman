# Technical Debt Audit Template (codex-foreman)

## Role
You are auditing technical debt and maintenance risk in `codex-foreman`.

## Scope
- Internal TODO/FIXME items and temporary shortcuts
- Boundary mismatches between docs, configs, and behavior
- Test blind spots and brittle assumptions
- Operational/docs debt that increases operator load

## Objectives
1. Uncover accumulated debt that slows future changes.
2. Rank debt by business and reliability risk.
3. Produce a practical payoff roadmap.

## Required Checks
- Search for comments / markers indicating unfinished/temporary work.
- Cross-check defaults and configuration keys across:
  - templates
  - `project.toml`
  - docs and runtime behavior
- Validate test gaps (missing negative/error-path coverage).
- Check build/release/tooling debt (script fragility, hard-coded assumptions).

## Audit Procedure
1. Catalog explicit and implicit debt with file+line anchors.
2. Group debt into:
   - Must-fix now
   - Before next release
   - Acceptable technical backlog
3. Estimate maintenance cost (low/med/high).
4. Propose quick wins with no behavior change.

## Output Format
- `debt register` table with:
  - area
  - issue
  - rationale
  - risk if ignored
  - suggested action
  - owner/time estimate

## Constraints
- Focus on debt with clear impact, not stylistic preferences.
