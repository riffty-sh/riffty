# Executive Acceptance Demo: MCP Stdio Diff Reviewer & Blast-Radius Auto-Healer (`US-006`)

## 1. Executive Summary
- **Problem Addressed**: AI coding agents (Claude Code, Cursor, Codex) waste thousands of LLM tokens reading full-repo lint noise or miss cross-file caller breakages when editing function signatures.
- **Delivered Capability**: `riffty mcp` (and `riffty_mcp`) exposes `riffty_review_changes` over stdio JSON-RPC (`rmcp` 3.10.0). `ReviewFacade` extracts touched `TextRange`s from `git diff`, expands blast radius across the repository via `TypeOraclePort::reverse_dependents`, suppresses untouched baseline diagnostics, heals `Applicability::Safe` issues directly on disk when `auto_heal: true`, and returns a deterministic `APPROVED` / `REQUEST_CHANGES` JSON verdict.

---

## 2. Live Executed Stdio JSON-RPC Wire Transcript

### Scenario Setup
1. **Committed Baseline (`main`)**:
   - `stats.py` has an untouched legacy `warnings.warn("baseline legacy warning")` on line 6 and `def calculate_average(values, match=None):`.
   - `test_stats.py` has an untouched `user_id in allowed` (`RFT-015`) on line 8 and calls `np.testing.assert_warns(RuntimeWarning, calculate_average, [], match="Mean of empty slice")` on line 12.
2. **Unstaged Developer Edit (touching `stats.py` lines 8–12 ONLY)**:
   - Developer removes `match=None` from `def calculate_average(values):` and adds `warnings.warn("Mean of empty slice", RuntimeWarning)`.
   - Notice `test_stats.py` has **zero lines in `git diff`**, yet its call to `calculate_average` via `np.testing.assert_warns(..., match=...)` is now broken!

### Wire Step 1 — Dry-Run Review (`auto_heal: false`)
```json
{
  "verdict": "REQUEST_CHANGES",
  "summary": {
    "files_reviewed": 2,
    "auto_healed_count": 0,
    "blocking_issues": 2
  },
  "auto_healed": [],
  "blocking_comments": [
    {
      "id": "rev-1",
      "file": "stats.py",
      "line": 10,
      "rule": "RFT-118",
      "severity": "WARNING",
      "message": "`warnings.warn(...)` called without `stacklevel=`; warning points to `warn()` site instead of caller.",
      "type_proof": {
        "injected_kwarg": "stacklevel=2",
        "resolved_fqname": "warnings.warn"
      },
      "suggested_action": "Added stacklevel=2 to warnings.warn()"
    },
    {
      "id": "rev-2",
      "file": "test_stats.py",
      "line": 12,
      "rule": "RFT-301",
      "severity": "ERROR",
      "message": "np.testing.assert_warns forwards `match=` to `calculate_average`, which does not accept `match`.",
      "type_proof": {
        "accepted_params": ["values"],
        "target_callable": "calculate_average",
        "unexpected_kwarg": "match"
      },
      "suggested_action": "Migrate to `with pytest.warns(RuntimeWarning, match=...):`."
    }
  ]
}
```

### Wire Step 2 — Autonomous Diff Healing (`auto_heal: true`)
```json
{
  "verdict": "APPROVED",
  "summary": {
    "files_reviewed": 2,
    "auto_healed_count": 2,
    "blocking_issues": 0
  },
  "auto_healed": [
    {
      "file": "stats.py",
      "line": 10,
      "rule": "RFT-118",
      "action": "Added stacklevel=2 to warnings.warn()"
    },
    {
      "file": "test_stats.py",
      "line": 12,
      "rule": "RFT-301",
      "action": "Migrate to `with pytest.warns(RuntimeWarning, match=...):`."
    }
  ],
  "blocking_comments": []
}
```

---

## 3. Acceptance Criteria Scorecard

| Criterion | Policy Rule | Automated E2E Test | Result |
| :--- | :---: | :--- | :---: |
| Untouched baseline diagnostics (`stats.py:6`, `test_stats.py:8`) suppressed | `R14` | `mcp_stdio_review_changes_diff_scoping_reverse_dependents_and_auto_heal` | **PASS** |
| Signature edit in `stats.py` audits caller in untouched `test_stats.py` | `R15` | `mcp_stdio_review_changes_diff_scoping_reverse_dependents_and_auto_heal` | **PASS** |
| `auto_heal: true` patches `Safe` fixes on disk while preserving `Unsafe` fixes | `R02`, `R10`, `R16` | `cli_check_and_fix_heals_safe_rules_and_preserves_unsafe_on_temp_repo` | **PASS** |
| Token-compact `APPROVED` / `REQUEST_CHANGES` JSON verdict schema | `R16` | `mcp_stdio_review_changes_diff_scoping_reverse_dependents_and_auto_heal` | **PASS** |

---

## 4. Traceability Anchors
- **User Story**: [US-006](file:///Users/jonatassouza/bin/dev/riffty_v2/docs/user-stories.md)
- **Technical Tasks**: `TASK-007`, `TASK-PROD-001`..`TASK-PROD-007`
- **Verified Affirmations**: `AFM-010`, `AFM-020`, `AFM-021`, `AFM-035`, `AFM-036`, `AFM-037`

