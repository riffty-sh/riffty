use std::path::PathBuf;
use std::process::Command;

use riffty_adapter_ast_grep::AstGrepAdapter;
use riffty_adapter_fs::FsWorkspaceAdapter;
use riffty_adapter_refaster::RefasterYamlRule;
use riffty_adapter_ruff_ast::{RuffAstAdapter, RuffImportAdapter};
use riffty_adapter_ty::{NullTypeOracle, TyOracleAdapter};
use riffty_core::{
    Applicability, CanonicalType, DiffTarget, FileId, GitDiffPort, ImportManagerPort, ImportSpec,
    NodeKind, PatchApplierPort, PatternMatcherPort, PrimitiveKind, Purity, SyntaxParserPort,
    TextRange, TriState, TypeOraclePort, WorkspaceFile, WorkspaceVfsPort,
};
use riffty_pipeline::{PipelineConfig, RifftyEngine};
use riffty_rules_builtin::builtin_rules;

#[test]
fn ruff_ast_and_import_adapter_conformance() {
    let parser = RuffAstAdapter;
    let src = "\"\"\"Module docstring.\"\"\"\nfrom __future__ import annotations\n\nasync def fetch(uid: int) -> str:\n    return str(uid)\n\ndef run() -> None:\n    fetch(1)\n";
    let tree = parser.parse(FileId(1), src).expect("valid Python");

    let kinds: Vec<NodeKind> = tree.iter_nodes().map(|n| n.kind).collect();
    assert!(kinds.contains(&NodeKind::Module));
    assert!(kinds.contains(&NodeKind::AsyncFunctionDef));
    assert!(kinds.contains(&NodeKind::FunctionDef));
    assert!(kinds.contains(&NodeKind::Call));
    assert!(kinds.contains(&NodeKind::Return));

    // Verify ImportManagerPort inserts after docstring + __future__ without AST pretty-printing
    let import_mgr = RuffImportAdapter;
    let edits = import_mgr.resolve_import_edits(
        src,
        &[
            ImportSpec {
                module: "pytest".to_string(),
                symbol: None,
            },
            ImportSpec {
                module: "__future__".to_string(),
                symbol: Some("annotations".to_string()),
            },
        ],
    );
    assert_eq!(edits.len(), 1);
    let mut patched = src.to_string();
    let e = &edits[0];
    patched.replace_range(e.range.start.0 as usize..e.range.end.0 as usize, &e.replacement);
    assert!(patched.starts_with("\"\"\"Module docstring.\"\"\"\nfrom __future__ import annotations\nimport pytest\n"));
}

#[test]
fn ty_salsa_oracle_and_null_oracle_conformance() {
    let f1_src = concat!(
        "import numpy as np\n",
        "def calculate_average(values: list[float]) -> float:\n",
        "    return 0.0\n",
        "def wrapper(*args, **kwargs) -> None:\n",
        "    pass\n",
        "async def send_mail(to: str) -> bool:\n",
        "    return True\n",
        "def check(x: str | None) -> int:\n",
        "    before = x\n",
        "    if x is None:\n",
        "        return 0\n",
        "    after = x\n",
        "    return len(after)\n",
        "call_target = np.testing.assert_warns\n",
    );
    let f2_src = concat!(
        "from stats import calculate_average\n",
        "res = calculate_average([1.0, 2.0])\n",
    );

    let oracle = TyOracleAdapter::new();
    oracle.index_workspace(&[
        WorkspaceFile {
            file_id: FileId(1),
            path: PathBuf::from("stats.py"),
            relative_path: "stats.py".to_string(),
            source: f1_src.to_string(),
        },
        WorkspaceFile {
            file_id: FileId(2),
            path: PathBuf::from("consumer.py"),
            relative_path: "consumer.py".to_string(),
            source: f2_src.to_string(),
        },
    ]);

    // 1. Resolve imported dotted FQN (`np.testing.assert_warns` -> `numpy.testing.assert_warns`)
    let np_pos = f1_src.find("np.testing.assert_warns").unwrap() as u32;
    let np_range = TextRange::new(np_pos, np_pos + "np.testing.assert_warns".len() as u32);
    assert_eq!(
        oracle.resolve_fqname(FileId(1), np_range).as_deref(),
        Some("numpy.testing.assert_warns")
    );

    // 2. Verify Callable `has_var_args` & `has_var_kwargs` & `is_async`
    let calc_def_pos = f1_src.find("def calculate_average").unwrap() as u32;
    // Query `calculate_average` usage in consumer.py (cross-file Salsa + export resolution!)
    let call_in_f2 = f2_src.rfind("calculate_average").unwrap() as u32;
    let calc_ty = oracle.type_of(
        FileId(2),
        TextRange::new(call_in_f2, call_in_f2 + "calculate_average".len() as u32),
    );
    match calc_ty {
        CanonicalType::Callable {
            params,
            has_var_args,
            has_var_kwargs,
            is_async,
            ..
        } => {
            assert_eq!(params.len(), 1);
            assert_eq!(params[0].name, "values");
            assert!(!has_var_args);
            assert!(!has_var_kwargs);
            assert!(!is_async);
        }
        other => panic!("expected Callable for calculate_average, got {other:?}"),
    }

    // 3. Verify `reverse_dependents` finds cross-file callers in `consumer.py` when `calculate_average` changes!
    let rev = oracle.reverse_dependents(
        FileId(1),
        &[TextRange::new(calc_def_pos, calc_def_pos + 30)],
    );
    assert!(!rev.is_empty());
    assert!(rev.iter().any(|(fid, _)| *fid == FileId(2)));

    // 4. Verify CFG `can_be_none` narrowing after early `if x is None: return 0`
    let before_x = f1_src.find("before = x").unwrap() as u32 + "before = ".len() as u32;
    let after_x = f1_src.find("after = x").unwrap() as u32 + "after = ".len() as u32;
    assert_eq!(
        oracle.can_be_none(FileId(1), TextRange::new(before_x, before_x + 1)),
        TriState::Yes
    );
    assert_eq!(
        oracle.can_be_none(FileId(1), TextRange::new(after_x, after_x + 1)),
        TriState::No
    );

    // 5. Verify `expression_purity`: `len(after)` is Pure whereas `calculate_average([1.0, 2.0])` is SideEffecting
    let len_call_pos = f1_src.find("len(after)").unwrap() as u32;
    assert_eq!(
        oracle.expression_purity(
            FileId(1),
            TextRange::new(len_call_pos, len_call_pos + "len(after)".len() as u32)
        ),
        Purity::Pure
    );
    let calc_call_pos = f2_src.find("calculate_average([1.0, 2.0])").unwrap() as u32;
    assert_eq!(
        oracle.expression_purity(
            FileId(2),
            TextRange::new(
                calc_call_pos,
                calc_call_pos + "calculate_average([1.0, 2.0])".len() as u32
            )
        ),
        Purity::SideEffecting
    );

    // 6. Verify NullTypeOracle fallback soundness (`R03`)
    let null_oracle = NullTypeOracle;
    assert_eq!(
        null_oracle.type_of(FileId(1), np_range),
        CanonicalType::Unknown
    );
    assert_eq!(
        null_oracle.is_subtype(
            &CanonicalType::Primitive(PrimitiveKind::Int),
            &CanonicalType::Primitive(PrimitiveKind::Str)
        ),
        TriState::Unknown
    );
}

#[test]
fn ast_grep_adapter_captures_single_and_variadic_metavars() {
    let matcher = AstGrepAdapter;
    let src = "a = old_call(x, 1, 2)\nb = old_call(y)\n";
    let matches = matcher
        .find_matches(src, "old_call($FIRST, $$$REST)")
        .expect("valid pattern");
    assert_eq!(matches.len(), 2);
    assert_eq!(matches[0].captures["FIRST"].range.slice(src), Some("x"));
    assert_eq!(matches[0].captures["REST"].range.slice(src), Some("1, 2"));
    assert_eq!(matches[1].captures["FIRST"].range.slice(src), Some("y"));
    assert!(matches[1].captures["REST"].range.is_empty());
}

#[test]
fn fs_workspace_and_git_diff_adapter_conformance() {
    let temp = std::env::temp_dir().join(format!(
        "riffty_v2_conformance_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&temp).unwrap();

    Command::new("git")
        .arg("init")
        .current_dir(&temp)
        .output()
        .unwrap();
    Command::new("git")
        .args(["config", "user.email", "test@riffty.dev"])
        .current_dir(&temp)
        .output()
        .unwrap();
    Command::new("git")
        .args(["config", "user.name", "Riffty Test"])
        .current_dir(&temp)
        .output()
        .unwrap();

    let py_file = temp.join("mod_a.py");
    std::fs::write(&py_file, "x = 1\ny = 2\n").unwrap();
    Command::new("git")
        .args(["add", "."])
        .current_dir(&temp)
        .output()
        .unwrap();
    Command::new("git")
        .args(["commit", "-m", "init"])
        .current_dir(&temp)
        .output()
        .unwrap();

    // Modify line 2 only
    std::fs::write(&py_file, "x = 1\ny = 99\n").unwrap();

    let fs_adapter = FsWorkspaceAdapter;
    let discovered = fs_adapter.discover_python_files(&temp).unwrap();
    assert_eq!(discovered.len(), 1);
    assert_eq!(discovered[0].relative_path, "mod_a.py");

    let diffs = fs_adapter
        .touched_ranges(&temp, &DiffTarget::Unstaged)
        .unwrap();
    assert_eq!(diffs.len(), 1);
    let touched_slice = diffs[0].touched_ranges[0]
        .slice("x = 1\ny = 99\n")
        .unwrap();
    assert_eq!(touched_slice, "y = 99\n");

    // Verify sandbox containment blocks writes outside workspace
    let outside = std::env::temp_dir().join("riffty_outside_probe.py");
    std::fs::write(&outside, "# probe\n").unwrap();
    let err = fs_adapter
        .write_patched_file(&temp, &outside, "# blocked\n")
        .unwrap_err();
    assert!(err.contains("security boundary violation"));

    let _ = std::fs::remove_file(&outside);
    let _ = std::fs::remove_dir_all(&temp);
}

#[test]
fn golden_scipy_20068_assert_warns_safe_vs_unsafe_and_comment_preservation() {
    let scipy_fixture = concat!(
        "\"\"\"SciPy #20068 golden regression fixture.\"\"\"\n",
        "import warnings\n",
        "import numpy as np\n",
        "\n",
        "# Header comment above calculate_average must survive byte-for-byte\n",
        "def calculate_average(values):\n",
        "    if len(values) == 0:\n",
        "        warnings.warn(\"Mean of empty slice\", RuntimeWarning, stacklevel=2)\n",
        "        return float(\"nan\")\n",
        "    return sum(values) / len(values)\n",
        "\n",
        "def decorated_wrapper(values, *args, **kwargs):\n",
        "    return calculate_average(values)\n",
        "\n",
        "def test_empty_slice_warning():\n",
        "    # Comment 1: explain why empty list triggers RuntimeWarning\n",
        "    np.testing.assert_warns(RuntimeWarning, calculate_average, [], match=\"Mean of empty slice\")  # Comment 2: inline trailing note\n",
        "    # Comment 3: **kwargs wrapper silently swallows match= in NumPy\n",
        "    np.testing.assert_warns(RuntimeWarning, decorated_wrapper, [], match=\"Mean of empty slice\")\n",
    );

    let parser = RuffAstAdapter;
    let oracle = TyOracleAdapter::new();
    let import_mgr = RuffImportAdapter;
    let matcher = AstGrepAdapter;

    // Step 1: Audit without applying fixes to verify exact `Applicability::Safe` vs `Applicability::Unsafe` & `type_proof`
    let audit_engine = RifftyEngine {
        parser: &parser,
        oracle: &oracle,
        import_manager: &import_mgr,
        pattern_matcher: Some(&matcher),
        rules: builtin_rules(),
    };
    let audit_res = audit_engine
        .run_workspace(
            &[WorkspaceFile {
                file_id: FileId(1),
                path: PathBuf::from("stats.py"),
                relative_path: "stats.py".to_string(),
                source: scipy_fixture.to_string(),
            }],
            PipelineConfig {
                apply_fixes: false,
                allow_unsafe_fixes: false,
                max_passes: 1,
            },
            None,
        )
        .expect("audit succeeds");

    let diags = &audit_res[0].remaining_diagnostics;
    assert_eq!(diags.len(), 2, "expected 2 RFT-301 diagnostics, got {diags:?}");

    // First call (`calculate_average(values)`): NO `match` param and `has_var_kwargs == false` => `Applicability::Safe`!
    let safe_diag = &diags[0];
    assert_eq!(safe_diag.rule_id, "RFT-301");
    assert_eq!(
        safe_diag.message,
        "np.testing.assert_warns forwards `match=` to `calculate_average`, which does not accept `match`."
    );
    assert_eq!(
        safe_diag.type_proof,
        Some(serde_json::json!({
            "target_callable": "calculate_average",
            "accepted_params": ["values"],
            "unexpected_kwarg": "match",
        }))
    );
    assert_eq!(
        safe_diag.fix.as_ref().expect("fix present").applicability,
        Applicability::Safe
    );

    // Second call (`decorated_wrapper(values, *args, **kwargs)`): `has_var_kwargs == true` => downgraded to `Applicability::Unsafe`!
    let unsafe_diag = &diags[1];
    assert_eq!(unsafe_diag.rule_id, "RFT-301");
    assert_eq!(
        unsafe_diag.fix.as_ref().expect("fix present").applicability,
        Applicability::Unsafe
    );

    // Step 2: Run `--fix` (`allow_unsafe_fixes = false`) and verify clean byte-offset rewrite, `import pytest` synthesis,
    // 100% comment preservation, and preservation of the `Unsafe` `decorated_wrapper` call untouched!
    let fix_engine = RifftyEngine {
        parser: &parser,
        oracle: &oracle,
        import_manager: &import_mgr,
        pattern_matcher: Some(&matcher),
        rules: builtin_rules(),
    };
    let fix_res = fix_engine
        .run_workspace(
            &[WorkspaceFile {
                file_id: FileId(1),
                path: PathBuf::from("stats.py"),
                relative_path: "stats.py".to_string(),
                source: scipy_fixture.to_string(),
            }],
            PipelineConfig {
                apply_fixes: true,
                allow_unsafe_fixes: false,
                max_passes: 10,
            },
            None,
        )
        .expect("fix succeeds");

    let healed = &fix_res[0].final_source;
    assert!(healed.contains("import pytest\n"), "missing `import pytest`:\n{healed}");
    assert!(
        healed.contains("# Header comment above calculate_average must survive byte-for-byte"),
        "lost header comment:\n{healed}"
    );
    assert!(
        healed.contains("# Comment 1: explain why empty list triggers RuntimeWarning"),
        "lost Comment 1:\n{healed}"
    );
    assert!(
        healed.contains(
            "    with pytest.warns(RuntimeWarning, match=\"Mean of empty slice\"):\n        calculate_average([])  # Comment 2: inline trailing note"
        ),
        "unexpected rewrite output:\n{healed}"
    );
    assert!(
        healed.contains("# Comment 3: **kwargs wrapper silently swallows match= in NumPy"),
        "lost Comment 3:\n{healed}"
    );
    assert!(
        healed.contains("np.testing.assert_warns(RuntimeWarning, decorated_wrapper, [], match=\"Mean of empty slice\")"),
        "Unsafe **kwargs call must NOT be auto-rewritten without --unsafe-fixes:\n{healed}"
    );
    assert_eq!(fix_res[0].remaining_diagnostics.len(), 1);
    assert_eq!(fix_res[0].remaining_diagnostics[0].rule_id, "RFT-301");
}

#[test]
fn refaster_yaml_callable_accepts_kwarg_and_fqname_guards_conformance() {
    let yaml = r#"
rules:
  - id: RFT-YAML-310
    name: custom-runner-timeout
    severity: warning
    applicability: safe
    message: "Migrate legacy `run_job($FN, $ARG)` to `$FN($ARG, timeout=30)`"
    pattern: "runner.run_job($FN, $ARG)"
    guards:
      - kind: fqname_in
        allowed: ["pkg.runner.run_job"]
      - kind: callable_accepts_kwarg
        target: "$FN"
        kwarg: "timeout"
        expected: true
    replace: "$FN($ARG, timeout=30)"
"#;
    let rules = RefasterYamlRule::from_yaml_str(yaml).expect("valid yaml");
    let src = concat!(
        "from pkg import runner\n",
        "\n",
        "def accepts_timeout(url: str, timeout: int = 10) -> None:\n",
        "    pass\n",
        "\n",
        "def no_timeout(url: str) -> None:\n",
        "    pass\n",
        "\n",
        "runner.run_job(accepts_timeout, 'https://a.example')\n",
        "runner.run_job(no_timeout, 'https://b.example')\n",
    );

    let parser = RuffAstAdapter;
    let oracle = TyOracleAdapter::new();
    let import_mgr = RuffImportAdapter;
    let matcher = AstGrepAdapter;
    let engine = RifftyEngine {
        parser: &parser,
        oracle: &oracle,
        import_manager: &import_mgr,
        pattern_matcher: Some(&matcher),
        rules,
    };

    let res = engine
        .run_workspace(
            &[WorkspaceFile {
                file_id: FileId(1),
                path: PathBuf::from("jobs.py"),
                relative_path: "jobs.py".to_string(),
                source: src.to_string(),
            }],
            PipelineConfig {
                apply_fixes: true,
                allow_unsafe_fixes: false,
                max_passes: 5,
            },
            None,
        )
        .unwrap();

    let out = &res[0].final_source;
    assert!(out.contains("accepts_timeout('https://a.example', timeout=30)"), "out:\n{out}");
    assert!(out.contains("runner.run_job(no_timeout, 'https://b.example')"), "out:\n{out}");
}

#[test]
fn golden_airflow_70875_scoped_import_heals_shadowed_local_and_deprecated_module_import() {
    let yaml = r#"
rules:
  - id: RFT-AF-70875-MASK-SECRET
    name: airflow-70875-canonical-mask-secret
    severity: warning
    applicability: safe
    message: "Import `mask_secret` from canonical `airflow.sdk.log`."
    pattern: "mask_secret($$$ARGS)"
    guards:
      - kind: fqname_in
        allowed:
          - "airflow.sdk._shared.secrets_masker.mask_secret"
          - "airflow.utils.log.secrets_masker.mask_secret"
    replace: "mask_secret($$$ARGS)"
    imports:
      - module: "airflow.sdk.log"
        symbol: "mask_secret"
"#;
    let rules = RefasterYamlRule::from_yaml_str(yaml).expect("valid yaml");

    let config_src = concat!(
        "class AirflowSDKConfigParser:\n",
        "    def mask_secrets(self, val: str) -> None:\n",
        "        from airflow.sdk._shared.secrets_masker import mask_secret  # lazy import\n",
        "        mask_secret(val)\n",
    );
    let dag_src = concat!(
        "from airflow.utils.log.secrets_masker import mask_secret\n",
        "\n",
        "def run_dag(token: str) -> None:\n",
        "    mask_secret(token)\n",
    );

    let parser = RuffAstAdapter;
    let oracle = TyOracleAdapter::new();
    let import_mgr = RuffImportAdapter;
    let matcher = AstGrepAdapter;
    let engine = RifftyEngine {
        parser: &parser,
        oracle: &oracle,
        import_manager: &import_mgr,
        pattern_matcher: Some(&matcher),
        rules,
    };

    let res = engine
        .run_workspace(
            &[
                WorkspaceFile {
                    file_id: FileId(1),
                    path: PathBuf::from("task-sdk/src/airflow/sdk/configuration.py"),
                    relative_path: "task-sdk/src/airflow/sdk/configuration.py".to_string(),
                    source: config_src.to_string(),
                },
                WorkspaceFile {
                    file_id: FileId(2),
                    path: PathBuf::from("dags/legacy_shim_caller_dag.py"),
                    relative_path: "dags/legacy_shim_caller_dag.py".to_string(),
                    source: dag_src.to_string(),
                },
            ],
            PipelineConfig {
                apply_fixes: true,
                allow_unsafe_fixes: false,
                max_passes: 10,
            },
            None,
        )
        .expect("airflow 70875 pipeline succeeds");

    // 1. `configuration.py`: converged cleanly in <= 2 passes, local import rewritten in-place, zero module-top duplicate!
    assert!(
        res[0].passes_executed <= 2,
        "Expected <= 2 passes (no 10-pass oscillation!), got {}",
        res[0].passes_executed
    );
    assert_eq!(
        res[0].final_source,
        concat!(
            "class AirflowSDKConfigParser:\n",
            "    def mask_secrets(self, val: str) -> None:\n",
            "        from airflow.sdk.log import mask_secret  # lazy import\n",
            "        mask_secret(val)\n",
        )
    );
    assert!(res[0].remaining_diagnostics.is_empty());

    // 2. `legacy_shim_caller_dag.py`: deprecated single-symbol module import replaced in-place with zero F811/F401!
    assert_eq!(
        res[1].final_source,
        concat!(
            "from airflow.sdk.log import mask_secret\n",
            "\n",
            "def run_dag(token: str) -> None:\n",
            "    mask_secret(token)\n",
        )
    );
    assert!(res[1].remaining_diagnostics.is_empty());
}

#[test]
fn golden_langchain_38054_pep562_mro_and_polymorphic_compat_override_safety() {
    let yaml_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/open_issue_langchain_38054.yml");
    let mut rules = builtin_rules();
    rules.extend(RefasterYamlRule::from_yaml_file(&yaml_path).expect("valid langchain 38054 yaml"));

    let files = vec![
        WorkspaceFile {
            file_id: FileId(1),
            path: PathBuf::from("libs/core/langchain_core/__init__.py"),
            relative_path: "libs/core/langchain_core/__init__.py".to_string(),
            source: String::new(),
        },
        WorkspaceFile {
            file_id: FileId(2),
            path: PathBuf::from("libs/core/langchain_core/load/__init__.py"),
            relative_path: "libs/core/langchain_core/load/__init__.py".to_string(),
            source: concat!(
                "from typing import TYPE_CHECKING\n",
                "if TYPE_CHECKING:\n",
                "    from langchain_core.load.serializable import Serializable\n",
            )
            .to_string(),
        },
        WorkspaceFile {
            file_id: FileId(3),
            path: PathBuf::from("libs/core/langchain_core/load/serializable.py"),
            relative_path: "libs/core/langchain_core/load/serializable.py".to_string(),
            source: "from pydantic import BaseModel\n\nclass Serializable(BaseModel):\n    pass\n"
                .to_string(),
        },
        WorkspaceFile {
            file_id: FileId(4),
            path: PathBuf::from("libs/core/langchain_core/runnables/__init__.py"),
            relative_path: "libs/core/langchain_core/runnables/__init__.py".to_string(),
            source: String::new(),
        },
        WorkspaceFile {
            file_id: FileId(5),
            path: PathBuf::from("libs/core/langchain_core/runnables/base.py"),
            relative_path: "libs/core/langchain_core/runnables/base.py".to_string(),
            source: concat!(
                "from langchain_core.load import Serializable\n",
                "class RunnableSerializable(Serializable):\n    pass\n",
            )
            .to_string(),
        },
        WorkspaceFile {
            file_id: FileId(6),
            path: PathBuf::from("libs/core/langchain_core/prompts/__init__.py"),
            relative_path: "libs/core/langchain_core/prompts/__init__.py".to_string(),
            source: String::new(),
        },
        WorkspaceFile {
            file_id: FileId(7),
            path: PathBuf::from("libs/core/langchain_core/prompts/base.py"),
            relative_path: "libs/core/langchain_core/prompts/base.py".to_string(),
            source: concat!(
                "from langchain_core.runnables.base import RunnableSerializable\n",
                "\n",
                "class BasePromptTemplate(RunnableSerializable):\n",
                "    def _dict_for_compat(self):\n",
                "        return self.dict()\n",
            )
            .to_string(),
        },
        WorkspaceFile {
            file_id: FileId(8),
            path: PathBuf::from("libs/core/langchain_core/language_models/__init__.py"),
            relative_path: "libs/core/langchain_core/language_models/__init__.py".to_string(),
            source: String::new(),
        },
        WorkspaceFile {
            file_id: FileId(9),
            path: PathBuf::from("libs/core/langchain_core/language_models/llms.py"),
            relative_path: "libs/core/langchain_core/language_models/llms.py".to_string(),
            source: concat!(
                "from langchain_core.runnables.base import RunnableSerializable\n",
                "\n",
                "class BaseLLM(RunnableSerializable):\n",
                "    @property\n",
                "    def _identifying_params(self):\n",
                "        return self.dict()\n",
            )
            .to_string(),
        },
        WorkspaceFile {
            file_id: FileId(10),
            path: PathBuf::from("app/caller.py"),
            relative_path: "app/caller.py".to_string(),
            source: concat!(
                "from langchain_core.load import Serializable\n",
                "\n",
                "class ConcreteSpec(Serializable):\n",
                "    tool: str\n",
                "\n",
                "def export(raw: dict) -> tuple[dict, str]:\n",
                "    spec: ConcreteSpec = ConcreteSpec.parse_obj(raw)\n",
                "    return spec.dict(exclude_none=True), spec.json()\n",
            )
            .to_string(),
        },
    ];

    let parser = RuffAstAdapter;
    let oracle = TyOracleAdapter::new();
    let import_mgr = RuffImportAdapter;
    let matcher = AstGrepAdapter;
    let engine = RifftyEngine {
        parser: &parser,
        oracle: &oracle,
        import_manager: &import_mgr,
        pattern_matcher: Some(&matcher),
        rules,
    };

    let res = engine
        .run_workspace(
            &files,
            PipelineConfig {
                apply_fixes: true,
                allow_unsafe_fixes: false,
                max_passes: 5,
            },
            None,
        )
        .expect("langchain 38054 pipeline succeeds");

    // 1. `prompts/base.py` & `llms.py`: detected across PEP 562 `TYPE_CHECKING` MRO, downgraded to `Unsafe` (`self.dict()` preserved!)
    assert!(res[6].final_source.contains("return self.dict()"));
    assert_eq!(res[6].remaining_diagnostics.len(), 1);
    assert_eq!(
        res[6].remaining_diagnostics[0]
            .fix
            .as_ref()
            .map(|f| f.applicability),
        Some(Applicability::Unsafe)
    );

    assert!(res[8].final_source.contains("return self.dict()"));
    assert_eq!(res[8].remaining_diagnostics.len(), 1);
    assert_eq!(
        res[8].remaining_diagnostics[0]
            .fix
            .as_ref()
            .map(|f| f.applicability),
        Some(Applicability::Unsafe)
    );

    // 2. `app/caller.py`: closed concrete `ConcreteSpec` auto-healed cleanly!
    assert!(
        res[9]
            .final_source
            .contains("spec: ConcreteSpec = ConcreteSpec.model_validate(raw)")
    );
    assert!(
        res[9]
            .final_source
            .contains("return spec.model_dump(exclude_none=True), spec.model_dump_json()")
    );
    assert!(res[9].remaining_diagnostics.is_empty());
}

