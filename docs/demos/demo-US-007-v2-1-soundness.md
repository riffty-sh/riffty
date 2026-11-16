# Executive Acceptance Demo: Open-World Soundness, Polymorphic Dispatch Safety & Lexical-Scope Remediation (`US-007`)

## 1. Executive Summary
- **Problem Addressed**: Deep postmortem verification against 6 live upstream repositories (`langchain#38054`, `bump-pydantic#50`, `airflow#70875`, `superset`, `fastapi`, `scipy`) exposed 6 real-world architectural edge cases (`GAP-01` through `GAP-06`):
  1. `GAP-01` (`R17`): Blindly auto-fixing `self.dict()` -> `self.model_dump()` inside open base compatibility helpers (`BasePromptTemplate._dict_for_compat`, `BaseLLM._identifying_params`, `BaseChatModel._identifying_params`) bypasses custom subclass `def dict(self)` overrides.
  2. `GAP-02` (`R18`): Closed-world nominal disjointness (`StrEnum`, `@property` getters, local variable shadowing, `<class 'type'>`, `Literal[Rule.BP001]`, `dict[K, V]` subscripting, custom `__eq__`) caused `RFT-015` false positives across `bump-pydantic`, `airflow`, `scipy`, `superset`, and `fastapi`.
  3. `GAP-03` (`R19`): Module-top import synthesis failed to heal shadowed function-local imports (`airflow/sdk/configuration.py:170`) and left duplicate `F811`/`F401` deprecated imports.
  4. `GAP-04` (`R20`): `RFT-118` injected dead no-op `stacklevel=2` onto Python 3.12+ `warnings.warn(..., skip_file_prefixes=...)` calls, while narrow `skip_file_prefixes=(os.path.dirname(__file__),)` in `scipy/_lib/_util.py:580` went undetected.
  5. `GAP-05` (`R21`): `CanonicalSyntaxTree` lacked first-class `ClassDef`, `With`, `If`, and `Try` nodes.
  6. `GAP-06` (`R22`): `TyOracleAdapter` missed 4-hop MRO chains crossing PEP 562 `if TYPE_CHECKING:` lazy package re-exports (`langchain_core/load/__init__.py`) and monorepo package roots (`libs/core`, `task-sdk/src`).
- **Delivered Capability**: `riffty` v2.1 implements `OverrideRisk` + `not_overridden_below` / `not_inside_with` Refaster guards, open-world subtype/equality/subscript soundness, `ScopedImportRequest` in-place lexical import rewriting, `RFT-118` `skip_file_prefixes` mutual-exclusivity + `RFT-119`, 17-variant `NodeKind`, and PEP 562 `TYPE_CHECKING` + `--python-venv` MRO discovery — achieving **0 false positives** across all 12 benchmark scenarios.

---

## 2. Live Upstream Postmortem Verification Matrix (Before v2.0 vs After v2.1)

| Live Repository / Scenario | v2.0 Postmortem Failure Mode | v2.1 Verified Result (`target/debug/riffty`) |
| :--- | :--- | :--- |
| **`langchain-ai/langchain#38054`** (`open-case-01`, 195 files) | Rewrote `prompts/base.py:379` (`_dict_for_compat`) as `Safe`; missed `llms.py:1402` & `chat_models.py:2395` across `TYPE_CHECKING`; 4 `RFT-015` FPs | **0 `RFT-015` FPs** (real `usage.py:72` preserved); **discovers `prompts/base.py:379`, `llms.py:1402`, `chat_models.py:2395` across 4-hop `TYPE_CHECKING` MRO and classifies all 3 `[fix:Unsafe]`**; auto-heals 13 `Safe` fixes; MCP PR gate **`APPROVED`**; **155/155 `langchain_core` tests pass** (including custom `PromptTemplate.dict()` override regression test) |
| **`pydantic/bump-pydantic#50`** (`open-case-02`, 46 files) | 10 `RFT-015` FPs on `class Rule(str, Enum)` + `Literal[Rule.BP001]` | **0 diagnostics / 0 FPs** across all 46 upstream files; `issue50_repro` migrates `BaseModel` inner `class Config` with 0 FPs on `Controller` (`5/5` `pytest -W error` pass) |
| **`apache/airflow#70875`** (`open-case-03`, 235 files) | `configuration.py:191,203` failed to converge in 10 passes due to local import shadowing; `F811`/`F401` duplicate imports in DAGs; 17 `RFT-015` FPs | **0 `RFT-015` FPs** across 235 files while preserving **1/1 real upstream bug** (`db_manager.py:331` `"version_table" not in node.keywords`); **`configuration.py:169` rewritten in-place in 2 passes** (`0` `ruff F401,F811` errors); `6/6` pytest pass |
| **`apache/superset`** (`open-case-04`, 56 files) | `RFT-015` FP on `<class 'tuple'>` and `dict[str, Any]` subscripting | **0 diagnostics / 0 FPs** across all 56 live Superset files; Pandas 3.0 CoW safe rewrites + `next_df()` purity block verified |
| **`fastapi/fastapi`** (`open-case-05`, 52 files) | `RFT-015` FPs on `<class 'type'>` | **0 diagnostics / 0 FPs** across all 52 live FastAPI files; `FAST` & async contract repro verified |
| **`scipy/scipy`** (`open-case-06`, 60 files) | 7 `RFT-015` FPs (`@property`, shadowed local `mode`); dead `stacklevel=2` injected onto `skip_file_prefixes=` at `_lib/_util.py:584` | **0 `RFT-015` FPs**, **0 dead `stacklevel=2` injections**, auto-heals **20/20** missing `stacklevel=2` sites, and **`RFT-119` pinpoints `scipy/_lib/_util.py:580:9`** (`6/6` runtime pytest pass) |

---

## 3. All 12 Scenarios Re-Verification Summary

| # | Scenario | Status | Key Verification Proof |
| :---: | :--- | :---: | :--- |
| 1 | `yc_expert_review/case-01-scipy-pr20068-assert-warns` | **PASS** | 15/15 `assert_warns` sites healed identically to SciPy PR #20068; `**kwargs` downgraded to `Unsafe` |
| 2 | `yc_expert_review/case-02-pydantic-v1-v2-migration` | **PASS** | 11/11 `BaseModel` sites migrated; 0/10 non-Pydantic FPs; `3 passed` under `-W error::DeprecationWarning` |
| 3 | `yc_expert_review/case-03-sqlalchemy-14-to-20-codemod` | **PASS** | `Session` & subclass MRO migrated; non-SQLAlchemy `.query()` untouched; `6 passed` |
| 4 | `yc_expert_review/case-04-pandas3-cow-chained-assignment` | **PASS** | Superset `client_processing.py` lines 656 & 658 rewritten with `Purity::Pure` proof |
| 5 | `yc_expert_review/case-05-fastapi-annotated-and-async-contracts` | **PASS** | Async blocking I/O & tenant DB contracts enforced with 0 sync worker FPs |
| 6 | `yc_expert_review/case-06-mcp-pr-gate-cross-file-async` | **PASS** | `riffty_review_changes` expands `reverse_dependents`, heals `RFT-118`, blocks `RFT-004`/`RFT-015`, then approves fixed diff |
| 7 | `yc_open_issues_review/open-case-01-langchain-issue38054-pydantic-and-live-bugs` | **PASS** | 4-hop `TYPE_CHECKING` MRO + `OverrideRisk` `Unsafe` downgrade + MCP `APPROVED` + `155 passed` |
| 8 | `yc_open_issues_review/open-case-02-bump-pydantic-issue50-inner-config` | **PASS** | `0` FPs on 46 upstream `bump-pydantic` files + `5 passed` under `pytest -W error` |
| 9 | `yc_open_issues_review/open-case-03-airflow-issue70875-secrets-masker-and-head` | **PASS** | `configuration.py` local import healed in-place + `db_manager.py:331` caught + `0` FPs on 235 files |
| 10 | `yc_open_issues_review/open-case-04-superset-head-ruff29212-pandas3` | **PASS** | `0` FPs on 56 live Superset files + Pandas 3.0 CoW purity-guarded fixes verified |
| 11 | `yc_open_issues_review/open-case-05-fastapi-head-ruff16795-17226-21075` | **PASS** | `0` FPs on 52 live FastAPI files + `FAST` & async contract repro verified |
| 12 | `yc_open_issues_review/open-case-06-scipy-head-issue15765-ty2686-live-audit` | **PASS** | `0` `RFT-015` FPs + `20` `RFT-118` fixes + `RFT-119` catches `scipy/_lib/_util.py:580` + `6 passed` |

---

## 4. Traceability Anchors
- **User Story**: [US-007](file:///Users/jonatassouza/bin/dev/riffty_v2/docs/user-stories.md)
- **Technical Tasks**: [TASK-008](file:///Users/jonatassouza/bin/dev/riffty_v2/docs/technical-tasks.md), [TASK-009](file:///Users/jonatassouza/bin/dev/riffty_v2/docs/technical-tasks.md), [TASK-010](file:///Users/jonatassouza/bin/dev/riffty_v2/docs/technical-tasks.md)
- **Verified Affirmations**: `AFM-041` – `AFM-050` ([affirmation-matrix.md](file:///Users/jonatassouza/bin/dev/riffty_v2/docs/affirmation-matrix.md))

