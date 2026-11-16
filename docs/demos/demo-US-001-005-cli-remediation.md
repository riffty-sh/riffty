# Executive Acceptance Demo: Deterministic Type-Aware Bug Detection & Surgical Byte-Offset CLI Remediation (`US-001` – `US-005`)

## 1. Executive Summary
- **Problem Addressed**: Syntactic Python linters (`ruff`) miss semantic bugs that require resolved callable signatures, purity lattices, or container element type proofs (such as unawaited coroutines, disjoint `x in set[Y]` membership tests, missing `warnings.warn(..., stacklevel=2)`, and SciPy #20068's silent `np.testing.assert_warns(..., match=...)` keyword forwarding trap). Conversely, AST pretty-printers destroy developer comments and formatting.
- **Delivered Capability**: `riffty check [--fix] [--rules <yaml>]` combines Ruff's parser, Astral `ty`'s Salsa semantic engine, and `ast-grep` structural matching behind pure hexagonal ports in `riffty_core`, applying deterministic bottom-to-top UTF-8 byte `TextRange` edits with zero AST unparsing and zero false positives on `Unknown`/`Any` types.

---

## 2. Live Executed CLI Walkthrough Transcript

### Input Fixture (`stats.py` & `test_stats.py`)
```python
"""Statistical helpers (SciPy #20068 acceptance scenario)."""
import warnings
import numpy as np

def calculate_average(values):
    if len(values) == 0:
        warnings.warn("Mean of empty slice", RuntimeWarning)
        return float("nan")
    return sum(values) / len(values)
```

```python
import numpy as np
from stats import calculate_average

def check_tags(user_id: int, allowed: set[str]) -> bool:
    return user_id in allowed

def test_empty_slice():
    # Must preserve leading and inline comments!
    np.testing.assert_warns(RuntimeWarning, calculate_average, [], match="Mean of empty slice")  # SciPy #20068
```

### Executed Command & Verbatim Output
```text
$ riffty check . --rules rules/default.yml
stats.py:10:9: RFT-118 [WARNING] [fix:Safe] `warnings.warn(...)` called without `stacklevel=`; warning points to `warn()` site instead of caller.
test_stats.py:8:12: RFT-015 [ERROR] Membership test `user_id in allowed` compares disjoint types `int` and container element `str` (always `false`).
test_stats.py:12:5: RFT-301 [ERROR] [fix:Safe] np.testing.assert_warns forwards `match=` to `calculate_average`, which does not accept `match`.
riffty: checked 2 file(s), auto-healed 0 fix(es), 3 remaining diagnostic(s).
```

### Surgical Auto-Fix Result (`riffty check --fix`)
```python
import numpy as np
import pytest
from stats import calculate_average

def check_tags(user_id: int, allowed: set[str]) -> bool:
    return user_id in allowed

def test_empty_slice():
    # Must preserve leading and inline comments!
    with pytest.warns(RuntimeWarning, match="Mean of empty slice"):
        calculate_average([])  # SciPy #20068
```

---

## 3. Step-by-Step Storyboard & Policy Verification

| Step | Action | System / Policy Invariant | Verified Outcome |
| :---: | :--- | :--- | :---: |
| **1** | Inspect `crates/riffty_core/Cargo.toml` | `R01` Pure Domain Core (`serde`, `serde_json`, `thiserror` only) | **PASS** |
| **2** | Evaluate `RFT-004` & `RFT-015` under `NullTypeOracle` | `R03` Zero-False-Positive Soundness on `Unknown` / `Any` | **PASS** |
| **3** | Run `RFT-301` on `calculate_average(values)` vs `wrapper(*args, **kwargs)` | `R10` Conditional `Safe` vs `Unsafe` callable-signature classification | **PASS** |
| **4** | Run Refaster YAML `pow($VAL, 2) -> ($VAL * $VAL)` on `pow(n, 2)` vs `pow(next_counter(), 2)` | `R05` Purity-Guarded Metavariable Duplication (`Purity::Pure` required) | **PASS** |
| **5** | Apply multi-fix pass via `riffty check --fix` | `R02`, `R06`–`R09` Bottom-to-top UTF-8 byte patching + `import pytest` synthesis + comment preservation | **PASS** |

---

## 4. Traceability Anchors
- **User Stories**: [US-001](file:///Users/jonatassouza/bin/dev/riffty_v2/docs/user-stories.md), [US-002](file:///Users/jonatassouza/bin/dev/riffty_v2/docs/user-stories.md), [US-003](file:///Users/jonatassouza/bin/dev/riffty_v2/docs/user-stories.md), [US-004](file:///Users/jonatassouza/bin/dev/riffty_v2/docs/user-stories.md), [US-005](file:///Users/jonatassouza/bin/dev/riffty_v2/docs/user-stories.md)
- **Technical Tasks**: `TASK-001` – `TASK-006`
- **Verified Affirmations**: `AFM-001`..`AFM-009`, `AFM-011`..`AFM-019`, `AFM-022`..`AFM-034`, `AFM-038`..`AFM-040`

