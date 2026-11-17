use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use riffty_core::{
    CanonicalType, FileId, OverrideRisk, ParamInfo, PrimitiveKind, Purity, TextRange, TriState,
    TypeOraclePort, WorkspaceFile, eval_has_custom_eq_default, eval_is_subtype,
    eval_is_subtype_with, is_tuple_type_head, normalize_subscript_canonical_type,
    parse_bare_nominal_or_primitive, split_top_level_type_commas, split_top_level_type_pipe,
    strip_balanced_outer_parens,
};
use ruff_db::Db as SourceDb;
use ruff_db::diagnostic::Diagnostic as RuffDiagnostic;
use ruff_db::files::{File as DbFile, Files, system_path_to_file};
use ruff_db::parsed::parsed_module;
use ruff_db::system::{
    DbWithTestSystem, DbWithWritableSystem as _, System, SystemPathBuf, TestSystem,
};
use ruff_db::vendored::VendoredFileSystem;
use ruff_python_ast::{CmpOp, Expr, ExprRef, PythonVersion, Stmt};
use ruff_text_size::Ranged;
use ty_module_resolver::{Db as ModuleResolverDb, FallibleStrategy, SearchPathSettings};
use ty_python_core::platform::PythonPlatform;
use ty_python_core::program::ProgramSettings;
use ty_python_core::{Db as PythonCoreDb, ProgramFile, TestProgramDb};
use ty_python_semantic::dependency::DependencyMetadata;
use ty_python_semantic::lint::{LintRegistry, RuleSelection};
use ty_python_semantic::types::Type as TyType;
use ty_python_semantic::{
    AnalysisSettings, Db as TySemanticDb, HasType, ProgramEnvironment, SemanticModel,
    check_file_unwrap, default_lint_registry,
};
use ty_site_packages::{PythonVersionSource, PythonVersionWithSource};

fn to_core_range(range: ruff_text_size::TextRange) -> TextRange {
    TextRange::new(range.start().to_u32(), range.end().to_u32())
}

/// Hermetic Salsa database hosting Astral's `ty` semantic solver (`ty_python_semantic` + `ty_vendored`).
#[salsa::db]
#[derive(Clone)]
struct RifftySalsaDb {
    storage: salsa::Storage<Self>,
    files: Files,
    system: TestSystem,
    vendored: VendoredFileSystem,
    rule_selection: Arc<RuleSelection>,
    analysis_settings: Arc<AnalysisSettings>,
    program_settings: ProgramSettings,
}

impl RifftySalsaDb {
    fn from_workspace_files(
        workspace_files: &[WorkspaceFile],
        extra_roots: &[String],
        venv_path: Option<&Path>,
    ) -> Self {
        let vendored = ty_vendored::file_system().clone();
        let program_settings = ProgramSettings::empty(&vendored);
        let mut db = Self {
            storage: salsa::Storage::default(),
            files: Files::default(),
            system: TestSystem::default(),
            vendored,
            rule_selection: Arc::new(RuleSelection::from_registry(default_lint_registry())),
            analysis_settings: Arc::new(AnalysisSettings::default()),
            program_settings,
        };

        let src_root = SystemPathBuf::from("/src");
        let _ = db.memory_file_system().create_directory_all(&src_root);

        for wf in workspace_files {
            let vpath = virtual_src_path(&wf.relative_path);
            if let Some(parent) = vpath.parent() {
                let _ = db.memory_file_system().create_directory_all(parent);
            }
            let _ = db.write_file(&vpath, &wf.source);
        }

        let mut search_roots = vec![src_root];
        for prefix in extra_roots {
            if !prefix.is_empty() {
                let extra_sys = SystemPathBuf::from(format!("/src/{prefix}"));
                let _ = db.memory_file_system().create_directory_all(&extra_sys);
                if !search_roots.contains(&extra_sys) {
                    search_roots.push(extra_sys);
                }
            }
        }

        if let Some(venv) = venv_path
            && let Some(site_pkgs) = discover_venv_site_packages(venv)
        {
            let vsite_root = SystemPathBuf::from("/site-packages");
            let _ = db.memory_file_system().create_directory_all(&vsite_root);
            mount_venv_stubs(&mut db, &site_pkgs, &vsite_root, 1500);
            search_roots.push(vsite_root);
        }

        if let Ok(search_paths) = SearchPathSettings::new(search_roots).to_search_paths(
            db.system(),
            db.vendored(),
            &FallibleStrategy,
        ) {
            search_paths.try_register_static_roots(&db);
            db.program_settings = ProgramSettings {
                python_environment: Ok(None),
                python_version: PythonVersionWithSource {
                    version: PythonVersion::PY312,
                    source: PythonVersionSource::default(),
                },
                python_platform: PythonPlatform::default(),
                search_paths,
            };
        }
        db
    }
}

fn discover_venv_site_packages(venv: &Path) -> Option<PathBuf> {
    if venv.file_name().is_some_and(|n| n == "site-packages") && venv.is_dir() {
        return Some(venv.to_path_buf());
    }
    let win_sp = venv.join("Lib").join("site-packages");
    if win_sp.is_dir() {
        return Some(win_sp);
    }
    let lib_dir = venv.join("lib");
    if let Ok(entries) = std::fs::read_dir(&lib_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name.to_string_lossy().starts_with("python") {
                let sp = entry.path().join("site-packages");
                if sp.is_dir() {
                    return Some(sp);
                }
            }
        }
    }
    None
}

fn mount_venv_stubs(
    db: &mut RifftySalsaDb,
    host_dir: &Path,
    virtual_root: &SystemPathBuf,
    max_files: usize,
) {
    let mut mounted = 0usize;
    let mut stack = vec![host_dir.to_path_buf()];
    while let Some(cur) = stack.pop() {
        if mounted >= max_files {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&cur) else {
            continue;
        };
        for entry in entries.flatten() {
            if mounted >= max_files {
                break;
            }
            let p = entry.path();
            let fname = entry.file_name();
            let fname_str = fname.to_string_lossy();
            if p.is_dir() {
                if fname_str != "__pycache__" && !fname_str.ends_with(".dist-info") {
                    stack.push(p);
                }
            } else if (fname_str.ends_with(".pyi") || fname_str == "__init__.py")
                && let Ok(rel) = p.strip_prefix(host_dir)
                && let Ok(content) = std::fs::read_to_string(&p)
            {
                let rel_slash = rel.to_string_lossy().replace('\\', "/");
                let vpath = SystemPathBuf::from(format!("{virtual_root}/{rel_slash}"));
                if let Some(parent) = vpath.parent() {
                    let _ = db.memory_file_system().create_directory_all(parent);
                }
                let _ = db.write_file(&vpath, &content);
                mounted += 1;
            }
        }
    }
}

impl DbWithTestSystem for RifftySalsaDb {
    fn test_system(&self) -> &TestSystem {
        &self.system
    }

    fn test_system_mut(&mut self) -> &mut TestSystem {
        &mut self.system
    }
}

#[salsa::db]
impl SourceDb for RifftySalsaDb {
    fn vendored(&self) -> &VendoredFileSystem {
        &self.vendored
    }

    fn system(&self) -> &dyn System {
        &self.system
    }

    fn files(&self) -> &Files {
        &self.files
    }
}

#[salsa::db]
impl ModuleResolverDb for RifftySalsaDb {}

#[salsa::db]
impl PythonCoreDb for RifftySalsaDb {
    fn should_check_file(&self, file: DbFile) -> bool {
        !file.path(self).is_vendored_path()
    }
}

#[salsa::db]
impl TestProgramDb for RifftySalsaDb {
    fn program_settings(&self) -> &ProgramSettings {
        &self.program_settings
    }
}

#[salsa::db]
impl TySemanticDb for RifftySalsaDb {
    fn check_file(&self, file: DbFile) -> Vec<RuffDiagnostic> {
        if !self.should_check_file(file) {
            return Vec::new();
        }
        check_file_unwrap(self, self.program_file(file))
    }

    fn program_file(&self, file: DbFile) -> ProgramFile<'_> {
        self.program().program_file(self, file)
    }

    fn python_version_with_source(&self, _file: DbFile) -> &PythonVersionWithSource {
        &self.program_settings.python_version
    }

    fn rule_selection(&self, _file: DbFile) -> &RuleSelection {
        &self.rule_selection
    }

    fn lint_registry(&self) -> &LintRegistry {
        default_lint_registry()
    }

    fn analysis_settings(&self, _file: DbFile) -> &AnalysisSettings {
        &self.analysis_settings
    }

    fn dependency_metadata(&self, _file: DbFile) -> Option<&DependencyMetadata> {
        None
    }

    fn verbose(&self) -> bool {
        false
    }

    fn is_open_file(&self, _file: DbFile) -> bool {
        true
    }

    fn dyn_clone(&self) -> Box<dyn TySemanticDb> {
        Box::new(self.clone())
    }
}

#[salsa::db]
impl salsa::Database for RifftySalsaDb {}

fn virtual_src_path(relative_path: &str) -> SystemPathBuf {
    let clean = relative_path
        .replace('\\', "/")
        .trim_start_matches("./")
        .trim_start_matches('/')
        .to_string();
    SystemPathBuf::from(format!("/src/{clean}"))
}

fn clean_rel_slash(relative_path: &str) -> String {
    relative_path
        .replace('\\', "/")
        .trim_start_matches("./")
        .trim_start_matches('/')
        .to_string()
}

fn module_name_from_rel_path(relative_path: &str) -> String {
    let clean = clean_rel_slash(relative_path)
        .trim_end_matches(".py")
        .to_string();
    if let Some(pkg) = clean.strip_suffix("/__init__") {
        pkg.replace('/', ".")
    } else if clean == "__init__" {
        String::new()
    } else {
        clean.replace('/', ".")
    }
}

/// Discovers canonical Python module names by climbing contiguous `__init__.py` package directories (`R22`),
/// enabling monorepo layouts (`libs/core/langchain_core/...`, `task-sdk/src/airflow/sdk/...`) to resolve
/// canonical package FQNs (`langchain_core.*`, `airflow.sdk.*`) regardless of invocation root.
fn compute_canonical_module_map(files: &[WorkspaceFile]) -> (HashMap<FileId, String>, Vec<String>) {
    let mut init_dirs: HashSet<String> = HashSet::new();
    for wf in files {
        let rel = clean_rel_slash(&wf.relative_path);
        if rel == "__init__.py" {
            init_dirs.insert(String::new());
        } else if let Some(dir) = rel.strip_suffix("/__init__.py") {
            init_dirs.insert(dir.to_string());
        }
    }

    let mut mod_map = HashMap::with_capacity(files.len());
    let mut extra_roots_set: HashSet<String> = HashSet::new();

    for wf in files {
        let rel = clean_rel_slash(&wf.relative_path);
        let parts: Vec<&str> = rel.split('/').collect();
        if parts.len() <= 1 {
            mod_map.insert(wf.file_id, module_name_from_rel_path(&rel));
            continue;
        }

        let file_dir_parts = &parts[..parts.len() - 1];
        let mut pkg_start_idx: Option<usize> = None;
        // Walk from deepest directory upward while directories contain `__init__.py`
        let mut idx = file_dir_parts.len();
        while idx > 0 {
            let candidate_dir = file_dir_parts[..idx].join("/");
            if init_dirs.contains(&candidate_dir) {
                pkg_start_idx = Some(idx - 1);
                idx -= 1;
            } else {
                break;
            }
        }

        if let Some(start_idx) = pkg_start_idx {
            if start_idx > 0 {
                extra_roots_set.insert(parts[..start_idx].join("/"));
            }
            let sub_rel = parts[start_idx..].join("/");
            mod_map.insert(wf.file_id, module_name_from_rel_path(&sub_rel));
        } else {
            // Fallback for namespace / flat `src/<pkg>/...` layouts without `__init__.py`
            if parts.first() == Some(&"src") && parts.len() >= 2 {
                extra_roots_set.insert("src".to_string());
                let sub_rel = parts[1..].join("/");
                mod_map.insert(wf.file_id, module_name_from_rel_path(&sub_rel));
            } else {
                mod_map.insert(wf.file_id, module_name_from_rel_path(&rel));
            }
        }
    }

    let mut extra_roots: Vec<String> = extra_roots_set.into_iter().collect();
    extra_roots.sort();
    (mod_map, extra_roots)
}

#[derive(Debug, Clone)]
struct DefinedSymbol {
    symbol_fqn: String,
    def_range: TextRange,
}

#[derive(Debug, Clone, Default)]
struct ClassInfo {
    direct_bases: Vec<String>,
    methods: HashSet<String>,
    is_final: bool,
    defines_custom_eq: bool,
}

#[derive(Debug, Clone, Default)]
struct FileSemanticSnapshot {
    expr_types: HashMap<TextRange, CanonicalType>,
    expr_fqnames: HashMap<TextRange, String>,
    expr_nullability: HashMap<TextRange, TriState>,
    expr_purity: HashMap<TextRange, Purity>,
    self_or_cls_exprs: HashSet<TextRange>,
    definitions: Vec<DefinedSymbol>,
    /// `(referenced_symbol_fqn, reference_range)` within this file (for cross-file `reverse_dependents`).
    symbol_usages: Vec<(String, TextRange)>,
}

/// Primary `TypeOraclePort` adapter powered by Astral's `ty` Salsa database (`ty_python_semantic`)
/// combined with exact callable signature (`has_var_args`, `has_var_kwargs`, `is_async`), purity,
/// CFG nullability, polymorphic override risk, and cross-file `reverse_dependents` indexing.
#[derive(Default)]
pub struct TyOracleAdapter {
    snapshots: RwLock<HashMap<FileId, FileSemanticSnapshot>>,
    class_bases: RwLock<HashMap<String, Vec<String>>>,
    class_infos: RwLock<HashMap<String, ClassInfo>>,
    reexports: RwLock<HashMap<String, String>>,
    unique_short_classes: RwLock<HashMap<String, String>>,
    venv_path: Option<PathBuf>,
}

impl TyOracleAdapter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_venv(venv_path: Option<PathBuf>) -> Self {
        Self {
            venv_path,
            ..Self::default()
        }
    }

    /// Convenience helper for single-file unit/conformance tests.
    pub fn index_single_file(&self, file_id: FileId, relative_path: &str, source: &str) {
        self.index_workspace(&[WorkspaceFile {
            file_id,
            path: PathBuf::from(relative_path),
            relative_path: relative_path.to_string(),
            source: source.to_string(),
        }]);
    }

    fn normalize_class_fqn(
        &self,
        raw_fqn: &str,
        reexports: &HashMap<String, String>,
        short_map: &HashMap<String, String>,
    ) -> String {
        let c = canonicalize_fqn(raw_fqn);
        let resolved = chase_reexport(reexports, &c);
        if !resolved.contains('.')
            && let Some(full) = short_map.get(&resolved)
        {
            return full.clone();
        }
        resolved
    }

    fn eval_mro_subtype(
        &self,
        sub: &CanonicalType,
        sup: &CanonicalType,
        bases: &HashMap<String, Vec<String>>,
        infos: &HashMap<String, ClassInfo>,
        reexports: &HashMap<String, String>,
        shorts: &HashMap<String, String>,
    ) -> TriState {
        eval_is_subtype_with(
            sub,
            sup,
            &mut |leaf_sub, leaf_sup| match (leaf_sub, leaf_sup) {
                (
                    CanonicalType::Nominal {
                        fqname: sub_fqn, ..
                    },
                    CanonicalType::Primitive(sup_p),
                ) => {
                    let sub_norm = self.normalize_class_fqn(sub_fqn, reexports, shorts);
                    if has_transitive_base(bases, &sub_norm, sup_p.fqname())
                        || (*sup_p == PrimitiveKind::Float
                            && has_transitive_base(bases, &sub_norm, "builtins.int"))
                    {
                        TriState::Yes
                    } else if is_mro_completely_closed(infos, &sub_norm, &mut HashSet::new()) {
                        TriState::No
                    } else {
                        TriState::Unknown
                    }
                }
                (
                    CanonicalType::Primitive(_),
                    CanonicalType::Nominal {
                        fqname: sup_fqn, ..
                    },
                )
                | (
                    CanonicalType::VariadicTuple(_),
                    CanonicalType::Nominal {
                        fqname: sup_fqn, ..
                    },
                ) => {
                    let sup_norm = self.normalize_class_fqn(sup_fqn, reexports, shorts);
                    if is_closed_concrete_non_abc_class(infos, &sup_norm) {
                        TriState::No
                    } else {
                        TriState::Unknown
                    }
                }
                (
                    CanonicalType::Nominal {
                        fqname: sub_fqn, ..
                    },
                    CanonicalType::VariadicTuple(_),
                ) => {
                    let sub_norm = self.normalize_class_fqn(sub_fqn, reexports, shorts);
                    if !has_transitive_base(bases, &sub_norm, "builtins.tuple")
                        && is_mro_completely_closed(infos, &sub_norm, &mut HashSet::new())
                    {
                        TriState::No
                    } else {
                        TriState::Unknown
                    }
                }
                (
                    CanonicalType::Nominal {
                        fqname: sub_fqn,
                        args: sub_args,
                    },
                    CanonicalType::Nominal {
                        fqname: sup_fqn,
                        args: sup_args,
                    },
                ) => {
                    let sub_norm = self.normalize_class_fqn(sub_fqn, reexports, shorts);
                    let sup_norm = self.normalize_class_fqn(sup_fqn, reexports, shorts);
                    if sub_norm == sup_norm {
                        if sup_args.is_empty() {
                            return TriState::Yes;
                        }
                        if sub_args.is_empty() {
                            return TriState::Unknown;
                        }
                        if sub_args.len() != sup_args.len() {
                            return TriState::No;
                        }
                        let mut acc = TriState::Yes;
                        for (a, b) in sub_args.iter().zip(sup_args.iter()) {
                            acc = acc
                                .and(self.eval_mro_subtype(a, b, bases, infos, reexports, shorts));
                            if acc == TriState::No {
                                return TriState::No;
                            }
                        }
                        return acc;
                    }
                    if has_transitive_base(bases, &sub_norm, &sup_norm) {
                        return if sup_args.is_empty() {
                            TriState::Yes
                        } else {
                            TriState::Unknown
                        };
                    }
                    if has_transitive_base(bases, &sub_norm, "builtins.str")
                        && self.eval_mro_subtype(
                            &CanonicalType::Primitive(PrimitiveKind::Str),
                            leaf_sup,
                            bases,
                            infos,
                            reexports,
                            shorts,
                        ) == TriState::Yes
                    {
                        return TriState::Yes;
                    }
                    if sup_args.is_empty()
                        && has_transitive_base(bases, &sub_norm, "builtins.tuple")
                        && eval_is_subtype(
                            &CanonicalType::Nominal {
                                fqname: "builtins.tuple".to_string(),
                                args: Vec::new(),
                            },
                            leaf_sup,
                        ) == TriState::Yes
                    {
                        return TriState::Yes;
                    }
                    if is_mro_completely_closed(infos, &sub_norm, &mut HashSet::new())
                        && is_closed_concrete_non_abc_class(infos, &sup_norm)
                    {
                        TriState::No
                    } else {
                        TriState::Unknown
                    }
                }
                _ => TriState::Unknown,
            },
        )
    }
}

fn chase_reexport(reexports: &HashMap<String, String>, start: &str) -> String {
    let mut cur = canonicalize_fqn(start);
    for _ in 0..8 {
        if let Some(next) = reexports.get(&cur) {
            let next_c = canonicalize_fqn(next);
            if next_c == cur {
                break;
            }
            cur = next_c;
        } else {
            break;
        }
    }
    cur
}

#[derive(Clone, Default)]
struct ScopeEnv {
    module_name: String,
    enclosing_class_fqn: Option<String>,
    vars: HashMap<String, CanonicalType>,
    non_none_narrowed: HashSet<String>,
    imports: HashMap<String, String>,
    functions: HashMap<String, CanonicalType>,
    workspace_exports: Arc<HashMap<String, CanonicalType>>,
    property_exports: Arc<HashMap<String, CanonicalType>>,
    class_bases: Arc<HashMap<String, Vec<String>>>,
    reexports: Arc<HashMap<String, String>>,
}

impl TypeOraclePort for TyOracleAdapter {
    fn index_workspace(&self, files: &[WorkspaceFile]) {
        let (canonical_mods, extra_roots) = compute_canonical_module_map(files);
        let db =
            RifftySalsaDb::from_workspace_files(files, &extra_roots, self.venv_path.as_deref());
        let program = db.program();
        let prog_env = ProgramEnvironment::from_program(program);

        // Pass 1a: Collect PEP 562 / `if TYPE_CHECKING:` re-exports across all workspace modules (`R22`).
        let mut reexports_map: HashMap<String, String> = HashMap::new();
        for wf in files {
            let Ok(parsed) = ruff_python_parser::parse_module(&wf.source) else {
                continue;
            };
            let mod_name = canonical_mods
                .get(&wf.file_id)
                .cloned()
                .unwrap_or_else(|| module_name_from_rel_path(&wf.relative_path));
            let module = parsed.into_syntax();
            let mut boot_env = ScopeEnv {
                module_name: mod_name.clone(),
                ..ScopeEnv::default()
            };
            collect_top_level_imports(&module.body, &mut boot_env);
            for (bound, target_fqn) in &boot_env.imports {
                let exported_key = if mod_name.is_empty() {
                    bound.clone()
                } else {
                    format!("{mod_name}.{bound}")
                };
                if exported_key != *target_fqn {
                    reexports_map.insert(exported_key, target_fqn.clone());
                }
            }
        }

        // Pass 1b: Collect exported top-level function/class signatures, properties, & class MRO metadata.
        let mut exports_map: HashMap<String, CanonicalType> = HashMap::new();
        let mut property_map: HashMap<String, CanonicalType> = HashMap::new();
        let mut bases_map: HashMap<String, Vec<String>> = HashMap::new();
        let mut class_infos_map: HashMap<String, ClassInfo> = HashMap::new();
        let mut short_counts: HashMap<String, Option<String>> = HashMap::new();

        for wf in files {
            let Ok(parsed) = ruff_python_parser::parse_module(&wf.source) else {
                continue;
            };
            let mod_name = canonical_mods
                .get(&wf.file_id)
                .cloned()
                .unwrap_or_else(|| module_name_from_rel_path(&wf.relative_path));
            let module = parsed.into_syntax();
            let mut boot_env = ScopeEnv {
                module_name: mod_name.clone(),
                reexports: Arc::new(reexports_map.clone()),
                ..ScopeEnv::default()
            };
            collect_top_level_imports(&module.body, &mut boot_env);
            for val in boot_env.imports.values_mut() {
                *val = chase_reexport(&reexports_map, val);
            }

            for stmt in &module.body {
                match stmt {
                    Stmt::FunctionDef(func) => {
                        let sig = build_callable_signature(func, &boot_env);
                        exports_map.insert(format!("{mod_name}.{}", func.name.id), sig);
                    }
                    Stmt::ClassDef(cls) => {
                        let cls_fqn = format!("{mod_name}.{}", cls.name.id);
                        reexports_map.remove(&cls_fqn);
                        short_counts
                            .entry(cls.name.id.to_string())
                            .and_modify(|e| *e = None)
                            .or_insert_with(|| Some(cls_fqn.clone()));

                        exports_map.insert(
                            cls_fqn.clone(),
                            CanonicalType::Nominal {
                                fqname: cls_fqn.clone(),
                                args: Vec::new(),
                            },
                        );

                        let is_final = cls.decorator_list.iter().any(|dec| {
                            dotted_expr_name(&dec.expression).is_some_and(|d| {
                                matches!(
                                    d.as_str(),
                                    "final" | "typing.final" | "typing_extensions.final"
                                )
                            })
                        });

                        let mut direct_bases = Vec::new();
                        if let Some(args) = &cls.arguments {
                            for base_expr in &args.args {
                                let base_ty = parse_annotation_expr(base_expr, &boot_env);
                                match base_ty {
                                    CanonicalType::Nominal { fqname: b_fqn, .. } => {
                                        let resolved_base = if !b_fqn.contains('.') {
                                            format!("{mod_name}.{b_fqn}")
                                        } else {
                                            chase_reexport(
                                                &reexports_map,
                                                &canonicalize_fqn(&b_fqn),
                                            )
                                        };
                                        if matches!(
                                            resolved_base.as_str(),
                                            "enum.StrEnum" | "StrEnum"
                                        ) {
                                            direct_bases.push("builtins.str".to_string());
                                            direct_bases.push("enum.Enum".to_string());
                                        } else if matches!(
                                            resolved_base.as_str(),
                                            "enum.IntEnum" | "IntEnum" | "enum.IntFlag" | "IntFlag"
                                        ) {
                                            direct_bases.push("builtins.int".to_string());
                                            direct_bases.push("enum.Enum".to_string());
                                        } else if matches!(
                                            resolved_base.as_str(),
                                            "typing.NamedTuple"
                                                | "NamedTuple"
                                                | "typing_extensions.NamedTuple"
                                        ) {
                                            direct_bases.push("builtins.tuple".to_string());
                                            direct_bases.push("typing.NamedTuple".to_string());
                                        } else {
                                            direct_bases.push(resolved_base);
                                        }
                                    }
                                    CanonicalType::Primitive(pk) => {
                                        direct_bases.push(pk.fqname().to_string());
                                    }
                                    _ => {}
                                }
                            }
                        }

                        let mut methods = HashSet::new();
                        let mut defines_custom_eq = false;

                        let mut class_attrs: HashMap<String, CanonicalType> = HashMap::new();
                        for cstmt in &cls.body {
                            if let Stmt::Assign(assign) = cstmt
                                && assign.targets.len() == 1
                                && let Expr::Name(target_name) = &assign.targets[0]
                                && let Some(rhs_fqn) = dotted_expr_name(&assign.value)
                            {
                                let resolved_rhs = resolve_imported_dotted(&rhs_fqn, &boot_env)
                                    .or_else(|| boot_env.imports.get(&rhs_fqn).cloned())
                                    .unwrap_or_else(|| format!("{mod_name}.{rhs_fqn}"));
                                let canonical_rhs = canonicalize_fqn(&resolved_rhs);
                                let attr_name = target_name.id.to_string();
                                let attr_ty = CanonicalType::Nominal {
                                    fqname: canonical_rhs,
                                    args: vec![],
                                };
                                exports_map
                                    .insert(format!("{cls_fqn}.{attr_name}"), attr_ty.clone());
                                class_attrs.insert(attr_name, attr_ty);
                            }
                        }

                        for cstmt in &cls.body {
                            if let Stmt::FunctionDef(mfunc) = cstmt {
                                let mname = mfunc.name.id.to_string();
                                methods.insert(mname.clone());
                                if mname == "__eq__" {
                                    defines_custom_eq = true;
                                }
                                let mut msig = build_method_signature(mfunc, &boot_env);
                                if let CanonicalType::Callable {
                                    ref mut return_type,
                                    ..
                                } = msig
                                    && matches!(
                                        return_type.as_ref(),
                                        CanonicalType::Unknown | CanonicalType::Any
                                    )
                                    && let Some(inferred) = infer_method_return_type_from_body(
                                        &mfunc.body,
                                        &cls_fqn,
                                        &class_attrs,
                                        &direct_bases,
                                        &bases_map,
                                        &exports_map,
                                    )
                                {
                                    **return_type = inferred;
                                }
                                let is_prop = mfunc.decorator_list.iter().any(|dec| {
                                    dotted_expr_name(&dec.expression).is_some_and(|d| {
                                        matches!(
                                            d.as_str(),
                                            "property"
                                                | "cached_property"
                                                | "functools.cached_property"
                                        )
                                    })
                                });
                                let member_fqn = format!("{cls_fqn}.{mname}");
                                if is_prop {
                                    let prop_ty = match msig {
                                        CanonicalType::Callable { return_type, .. } => *return_type,
                                        other => other,
                                    };
                                    property_map.insert(member_fqn, prop_ty);
                                } else {
                                    exports_map.insert(member_fqn, msig);
                                }
                            }
                        }

                        if !direct_bases.is_empty() {
                            bases_map.insert(cls_fqn.clone(), direct_bases.clone());
                        }
                        class_infos_map.insert(
                            cls_fqn,
                            ClassInfo {
                                direct_bases,
                                methods,
                                is_final,
                                defines_custom_eq,
                            },
                        );
                    }
                    _ => {}
                }
            }
        }

        // Normalize all `direct_bases` through resolved `reexports_map`
        for bases in bases_map.values_mut() {
            for b in bases.iter_mut() {
                *b = chase_reexport(&reexports_map, b);
            }
        }
        for info in class_infos_map.values_mut() {
            for b in &mut info.direct_bases {
                *b = chase_reexport(&reexports_map, b);
            }
        }

        let unique_shorts: HashMap<String, String> = short_counts
            .into_iter()
            .filter_map(|(k, v)| v.map(|full| (k, full)))
            .collect();

        let shared_exports = Arc::new(exports_map);
        let shared_properties = Arc::new(property_map);
        let shared_bases = Arc::new(bases_map.clone());
        let shared_reexports = Arc::new(reexports_map.clone());

        // Pass 2: Build per-file semantic snapshot enriched by Salsa `SemanticModel` (`ty`).
        let mut new_snapshots: HashMap<FileId, FileSemanticSnapshot> =
            HashMap::with_capacity(files.len());

        for wf in files {
            let vpath = virtual_src_path(&wf.relative_path);
            let mod_name = canonical_mods
                .get(&wf.file_id)
                .cloned()
                .unwrap_or_else(|| module_name_from_rel_path(&wf.relative_path));
            let mut snapshot = FileSemanticSnapshot::default();

            let mut env = ScopeEnv {
                module_name: mod_name.clone(),
                workspace_exports: Arc::clone(&shared_exports),
                property_exports: Arc::clone(&shared_properties),
                class_bases: Arc::clone(&shared_bases),
                reexports: Arc::clone(&shared_reexports),
                ..ScopeEnv::default()
            };

            if let Ok(db_file) = system_path_to_file(&db, &vpath) {
                let prog_file = ProgramFile::new(&db, db_file, program);
                let parsed_ref = parsed_module(&db, prog_file.python_file(&db)).load(&db);
                let model = SemanticModel::new(&db, prog_file);
                let suite = parsed_ref.suite();

                register_top_level_defs(suite, &mut env, &mut snapshot);
                walk_suite_with_ty(suite, &db, &prog_env, &model, &mut env, &mut snapshot);
            } else if let Ok(parsed) = ruff_python_parser::parse_module(&wf.source) {
                let module = parsed.into_syntax();
                register_top_level_defs(&module.body, &mut env, &mut snapshot);
                walk_suite_ast_only(&module.body, &mut env, &mut snapshot);
            }

            new_snapshots.insert(wf.file_id, snapshot);
        }

        *self.snapshots.write().expect("TyOracleAdapter lock") = new_snapshots;
        *self
            .class_bases
            .write()
            .expect("TyOracleAdapter bases lock") = bases_map;
        *self
            .class_infos
            .write()
            .expect("TyOracleAdapter infos lock") = class_infos_map;
        *self
            .reexports
            .write()
            .expect("TyOracleAdapter reexports lock") = reexports_map;
        *self
            .unique_short_classes
            .write()
            .expect("TyOracleAdapter shorts lock") = unique_shorts;
    }

    fn type_of(&self, file_id: FileId, range: TextRange) -> CanonicalType {
        let guard = self.snapshots.read().expect("TyOracleAdapter lock");
        guard
            .get(&file_id)
            .and_then(|s| s.expr_types.get(&range).cloned())
            .unwrap_or(CanonicalType::Unknown)
    }

    fn resolve_fqname(&self, file_id: FileId, range: TextRange) -> Option<String> {
        let guard = self.snapshots.read().expect("TyOracleAdapter lock");
        guard
            .get(&file_id)
            .and_then(|s| s.expr_fqnames.get(&range).cloned())
    }

    fn is_subtype(&self, sub: &CanonicalType, sup: &CanonicalType) -> TriState {
        let base_res = eval_is_subtype(sub, sup);
        if base_res != TriState::Unknown {
            return base_res;
        }

        let bases = self.class_bases.read().expect("TyOracleAdapter bases lock");
        let infos = self.class_infos.read().expect("TyOracleAdapter infos lock");
        let reexports = self
            .reexports
            .read()
            .expect("TyOracleAdapter reexports lock");
        let shorts = self
            .unique_short_classes
            .read()
            .expect("TyOracleAdapter shorts lock");

        self.eval_mro_subtype(sub, sup, &bases, &infos, &reexports, &shorts)
    }

    fn has_custom_eq(&self, ty: &CanonicalType) -> TriState {
        match ty {
            CanonicalType::Union(items) => {
                if items.is_empty() {
                    return TriState::Unknown;
                }
                let mut saw_unknown = false;
                for item in items {
                    match self.has_custom_eq(item) {
                        TriState::Yes => return TriState::Yes,
                        TriState::Unknown => saw_unknown = true,
                        TriState::No => {}
                    }
                }
                if saw_unknown {
                    TriState::Unknown
                } else {
                    TriState::No
                }
            }
            CanonicalType::Nominal { fqname, .. } => {
                let def = eval_has_custom_eq_default(ty);
                if def == TriState::No {
                    return TriState::No;
                }
                let infos = self.class_infos.read().expect("TyOracleAdapter infos lock");
                let reexports = self
                    .reexports
                    .read()
                    .expect("TyOracleAdapter reexports lock");
                let shorts = self
                    .unique_short_classes
                    .read()
                    .expect("TyOracleAdapter shorts lock");
                let norm = self.normalize_class_fqn(fqname, &reexports, &shorts);
                if is_stdlib_closed_standard_eq(&norm)
                    || is_unambiguous_stdlib_ast_node(&infos, &norm)
                {
                    return TriState::No;
                }
                eval_workspace_class_custom_eq(&infos, &norm, &mut HashSet::new())
            }
            _ => eval_has_custom_eq_default(ty),
        }
    }

    fn method_override_risk(
        &self,
        file_id: FileId,
        receiver_range: TextRange,
        method: &str,
        anchor_base_fqn: &str,
    ) -> OverrideRisk {
        let recv_ty = self.type_of(file_id, receiver_range);
        let CanonicalType::Nominal {
            fqname: recv_raw, ..
        } = &recv_ty
        else {
            return OverrideRisk::Unknown;
        };

        let anchor_ty = CanonicalType::Nominal {
            fqname: canonicalize_fqn(anchor_base_fqn),
            args: Vec::new(),
        };
        if self.is_subtype(&recv_ty, &anchor_ty) != TriState::Yes {
            return OverrideRisk::Unknown;
        }

        let infos = self.class_infos.read().expect("TyOracleAdapter infos lock");
        let bases = self.class_bases.read().expect("TyOracleAdapter bases lock");
        let reexports = self
            .reexports
            .read()
            .expect("TyOracleAdapter reexports lock");
        let shorts = self
            .unique_short_classes
            .read()
            .expect("TyOracleAdapter shorts lock");

        let recv_norm = self.normalize_class_fqn(recv_raw, &reexports, &shorts);
        let anchor_norm = self.normalize_class_fqn(anchor_base_fqn, &reexports, &shorts);

        // 1. Check if `recv_norm` or any MRO class strictly below `anchor_norm` overrides `method`.
        let mut stack = vec![recv_norm.as_str()];
        let mut visited = HashSet::new();
        while let Some(cur) = stack.pop() {
            if !visited.insert(cur)
                || cur == anchor_norm
                || matches!(
                    cur,
                    "abc.ABC"
                        | "ABC"
                        | "typing.Generic"
                        | "Generic"
                        | "typing_extensions.Generic"
                        | "builtins.object"
                        | "object"
                )
            {
                continue;
            }
            if let Some(info) = infos.get(cur) {
                if info.methods.contains(method) {
                    return OverrideRisk::PolymorphicOrOverridden;
                }
                for parent in &info.direct_bases {
                    stack.push(parent.as_str());
                }
            } else if let Some(parents) = bases.get(cur) {
                for parent in parents {
                    stack.push(parent.as_str());
                }
            } else {
                // Intermediate class in MRO is external/unindexed: cannot prove it doesn't override `method`.
                return OverrideRisk::Unknown;
            }
        }

        // 2. Open-world `self`/`cls` guard (`R17`): inside a non-`@final` class, downstream user subclasses
        //    may override `method`, so rewriting `self.method()` on the base class would bypass overrides!
        let is_self_or_cls = {
            let guard = self.snapshots.read().expect("TyOracleAdapter lock");
            guard
                .get(&file_id)
                .is_some_and(|s| s.self_or_cls_exprs.contains(&receiver_range))
        };
        if is_self_or_cls {
            let is_final = infos.get(&recv_norm).is_some_and(|i| i.is_final);
            if !is_final {
                return OverrideRisk::PolymorphicOrOverridden;
            }
        }

        // 3. Downward workspace subclass check (`R17`): if any indexed subclass of `recv_norm`
        //    overrides `method`, a variable/parameter typed as `recv_norm` may dynamically hold
        //    an instance of that overriding subclass.
        let is_recv_final = infos.get(&recv_norm).is_some_and(|i| i.is_final);
        if !is_recv_final {
            for (cls_fqn, info) in infos.iter() {
                if cls_fqn != &recv_norm
                    && info.methods.contains(method)
                    && has_transitive_base(&bases, cls_fqn, &recv_norm)
                {
                    return OverrideRisk::PolymorphicOrOverridden;
                }
            }
        }

        OverrideRisk::SealedEquivalent
    }

    fn can_be_none(&self, file_id: FileId, range: TextRange) -> TriState {
        let guard = self.snapshots.read().expect("TyOracleAdapter lock");
        let Some(snap) = guard.get(&file_id) else {
            return TriState::Unknown;
        };
        if let Some(&n) = snap.expr_nullability.get(&range) {
            return n;
        }
        match snap.expr_types.get(&range) {
            Some(CanonicalType::Unknown | CanonicalType::Any) | None => TriState::Unknown,
            Some(ty) if ty.includes_none() => TriState::Yes,
            Some(_) => TriState::No,
        }
    }

    fn expression_purity(&self, file_id: FileId, range: TextRange) -> Purity {
        let guard = self.snapshots.read().expect("TyOracleAdapter lock");
        let Some(snap) = guard.get(&file_id) else {
            return Purity::SideEffecting;
        };
        if let Some(&p) = snap.expr_purity.get(&range) {
            return p;
        }
        let mut saw_any = false;
        let mut worst = Purity::Pure;
        for (er, &ep) in &snap.expr_purity {
            if range.contains_range(*er) {
                saw_any = true;
                worst = worst.combine(ep);
            }
        }
        if saw_any {
            worst
        } else if range.is_empty() {
            Purity::Pure
        } else {
            Purity::SideEffecting
        }
    }

    fn reverse_dependents(
        &self,
        file_id: FileId,
        changed_ranges: &[TextRange],
    ) -> Vec<(FileId, TextRange)> {
        if changed_ranges.is_empty() {
            return Vec::new();
        }
        let guard = self.snapshots.read().expect("TyOracleAdapter lock");
        let Some(origin) = guard.get(&file_id) else {
            return Vec::new();
        };

        let mut modified_symbols: HashSet<&str> = HashSet::new();
        for def in &origin.definitions {
            if changed_ranges.iter().any(|cr| cr.intersects(def.def_range)) {
                modified_symbols.insert(def.symbol_fqn.as_str());
            }
        }
        if modified_symbols.is_empty() {
            return Vec::new();
        }

        let mut out: Vec<(FileId, TextRange)> = Vec::new();
        for (&other_fid, snap) in guard.iter() {
            if other_fid == file_id {
                continue;
            }
            for (sym_fqn, usage_range) in &snap.symbol_usages {
                if modified_symbols.contains(sym_fqn.as_str()) {
                    let pair = (other_fid, *usage_range);
                    if !out.contains(&pair) {
                        out.push(pair);
                    }
                }
            }
        }
        out.sort_by_key(|(fid, r)| (*fid, r.start, r.end));
        out
    }
}

fn is_stdlib_closed_standard_eq(fqn: &str) -> bool {
    fqn == "uuid.UUID"
        || fqn.starts_with("ast.")
        || fqn.starts_with("_ast.")
        || matches!(
            fqn,
            "builtins.list"
                | "builtins.set"
                | "builtins.frozenset"
                | "builtins.dict"
                | "builtins.tuple"
                | "builtins.type"
                | "datetime.datetime"
                | "datetime.date"
                | "datetime.time"
                | "datetime.timedelta"
                | "pathlib.Path"
        )
}

fn is_mro_completely_closed<'a>(
    infos: &'a HashMap<String, ClassInfo>,
    fqn: &'a str,
    visiting: &mut HashSet<&'a str>,
) -> bool {
    if matches!(
        fqn,
        "builtins.object"
            | "object"
            | "enum.Enum"
            | "Enum"
            | "builtins.str"
            | "builtins.int"
            | "builtins.float"
            | "builtins.bool"
            | "builtins.bytes"
            | "typing.Generic"
            | "Generic"
    ) || is_stdlib_closed_standard_eq(fqn)
        || is_unambiguous_stdlib_ast_node(infos, fqn)
    {
        return true;
    }
    if !visiting.insert(fqn) {
        return true;
    }
    let Some(info) = infos.get(fqn) else {
        return false;
    };
    info.direct_bases
        .iter()
        .all(|b| is_mro_completely_closed(infos, b.as_str(), visiting))
}

fn is_closed_concrete_non_abc_class(infos: &HashMap<String, ClassInfo>, fqn: &str) -> bool {
    if is_stdlib_closed_standard_eq(fqn) || is_unambiguous_stdlib_ast_node(infos, fqn) {
        return true;
    }
    let Some(info) = infos.get(fqn) else {
        return false;
    };
    if info.direct_bases.iter().any(|b| {
        matches!(
            b.as_str(),
            "abc.ABC" | "ABC" | "typing.Protocol" | "Protocol" | "typing_extensions.Protocol"
        )
    }) {
        return false;
    }
    is_mro_completely_closed(infos, fqn, &mut HashSet::new())
}

fn is_unambiguous_stdlib_ast_node(infos: &HashMap<String, ClassInfo>, fqn: &str) -> bool {
    if !matches!(
        fqn,
        "keyword" | "alias" | "comprehension" | "withitem" | "match_case"
    ) {
        return false;
    }
    let suffix = format!(".{fqn}");
    !infos.contains_key(fqn) && !infos.keys().any(|k| k.ends_with(&suffix))
}

fn eval_workspace_class_custom_eq<'a>(
    infos: &'a HashMap<String, ClassInfo>,
    fqn: &'a str,
    visiting: &mut HashSet<&'a str>,
) -> TriState {
    if matches!(
        fqn,
        "builtins.object"
            | "object"
            | "enum.Enum"
            | "Enum"
            | "builtins.str"
            | "builtins.int"
            | "builtins.float"
            | "builtins.bool"
            | "builtins.bytes"
            | "typing.Generic"
            | "Generic"
    ) || is_stdlib_closed_standard_eq(fqn)
    {
        return TriState::No;
    }
    if !visiting.insert(fqn) {
        return TriState::No;
    }
    let Some(info) = infos.get(fqn) else {
        return TriState::Unknown;
    };
    if info.defines_custom_eq {
        return TriState::Yes;
    }
    let mut saw_unknown = false;
    for b in &info.direct_bases {
        match eval_workspace_class_custom_eq(infos, b.as_str(), visiting) {
            TriState::Yes => return TriState::Yes,
            TriState::Unknown => saw_unknown = true,
            TriState::No => {}
        }
    }
    if saw_unknown {
        TriState::Unknown
    } else {
        TriState::No
    }
}

/// Zero-false-positive syntactic fallback (`R03`) returning `CanonicalType::Unknown` / `TriState::Unknown`.
#[derive(Default, Debug, Clone, Copy)]
pub struct NullTypeOracle;

impl TypeOraclePort for NullTypeOracle {
    fn type_of(&self, _file_id: FileId, _range: TextRange) -> CanonicalType {
        CanonicalType::Unknown
    }

    fn resolve_fqname(&self, _file_id: FileId, _range: TextRange) -> Option<String> {
        None
    }

    fn is_subtype(&self, _sub: &CanonicalType, _sup: &CanonicalType) -> TriState {
        TriState::Unknown
    }

    fn can_be_none(&self, _file_id: FileId, _range: TextRange) -> TriState {
        TriState::Unknown
    }

    fn expression_purity(&self, _file_id: FileId, range: TextRange) -> Purity {
        if range.is_empty() {
            Purity::Pure
        } else {
            Purity::SideEffecting
        }
    }

    fn reverse_dependents(
        &self,
        _file_id: FileId,
        _changed_ranges: &[TextRange],
    ) -> Vec<(FileId, TextRange)> {
        Vec::new()
    }
}

fn collect_top_level_imports(stmts: &[Stmt], env: &mut ScopeEnv) {
    for stmt in stmts {
        match stmt {
            Stmt::Import(imp) => {
                for alias in &imp.names {
                    let full = alias.name.id.to_string();
                    let bound = alias
                        .asname
                        .as_ref()
                        .map(|a| a.id.to_string())
                        .unwrap_or_else(|| full.split('.').next().unwrap_or(&full).to_string());
                    env.imports.insert(bound, full);
                }
            }
            Stmt::ImportFrom(imp) => {
                let base = resolve_import_from_base(imp, &env.module_name);
                for alias in &imp.names {
                    let sym = alias.name.id.as_str();
                    let bound = alias.asname.as_ref().map(|a| a.id.as_str()).unwrap_or(sym);
                    let fqn = if base.is_empty() {
                        sym.to_string()
                    } else {
                        chase_reexport(&env.reexports, &canonicalize_fqn(&format!("{base}.{sym}")))
                    };
                    env.imports.insert(bound.to_string(), fqn);
                }
            }
            Stmt::If(if_stmt) if is_type_checking_guard(&if_stmt.test, env) => {
                collect_top_level_imports(&if_stmt.body, env);
            }
            _ => {}
        }
    }
}

fn is_type_checking_guard(test: &Expr, env: &ScopeEnv) -> bool {
    let Some(dotted) = dotted_expr_name(test) else {
        return false;
    };
    if matches!(
        dotted.as_str(),
        "TYPE_CHECKING" | "typing.TYPE_CHECKING" | "typing_extensions.TYPE_CHECKING"
    ) {
        return true;
    }
    if let Some((head, "TYPE_CHECKING")) = dotted.split_once('.')
        && let Some(resolved_head) = env.imports.get(head)
    {
        return matches!(resolved_head.as_str(), "typing" | "typing_extensions");
    }
    false
}

fn resolve_import_from_base(imp: &ruff_python_ast::StmtImportFrom, current_module: &str) -> String {
    let raw_mod = imp.module.as_ref().map(|m| m.id.as_str()).unwrap_or("");
    if imp.level == 0 {
        return raw_mod.to_string();
    }
    let mut parts: Vec<&str> = current_module.split('.').collect();
    for _ in 0..imp.level {
        let _ = parts.pop();
    }
    if !raw_mod.is_empty() {
        parts.push(raw_mod);
    }
    parts.join(".")
}

fn register_top_level_defs(
    stmts: &[Stmt],
    env: &mut ScopeEnv,
    snapshot: &mut FileSemanticSnapshot,
) {
    collect_top_level_imports(stmts, env);
    for stmt in stmts {
        match stmt {
            Stmt::FunctionDef(func) => {
                let sig = build_callable_signature(func, env);
                let name = func.name.id.to_string();
                let fqn = format!("{}.{}", env.module_name, name);
                env.functions.insert(name.clone(), sig.clone());
                env.vars.insert(name, sig);
                snapshot.definitions.push(DefinedSymbol {
                    symbol_fqn: fqn,
                    def_range: to_core_range(func.range),
                });
            }
            Stmt::ClassDef(cls) => {
                let name = cls.name.id.to_string();
                let fqn = format!("{}.{}", env.module_name, name);
                env.vars.insert(
                    name,
                    CanonicalType::Nominal {
                        fqname: fqn.clone(),
                        args: Vec::new(),
                    },
                );
                for cstmt in &cls.body {
                    if let Stmt::FunctionDef(mfunc) = cstmt {
                        snapshot.definitions.push(DefinedSymbol {
                            symbol_fqn: format!("{fqn}.{}", mfunc.name.id),
                            def_range: to_core_range(mfunc.range),
                        });
                    }
                }
                snapshot.definitions.push(DefinedSymbol {
                    symbol_fqn: fqn,
                    def_range: to_core_range(cls.range),
                });
            }
            Stmt::Assign(assign) => {
                for target in &assign.targets {
                    if let Expr::Name(n) = target {
                        snapshot.definitions.push(DefinedSymbol {
                            symbol_fqn: format!("{}.{}", env.module_name, n.id),
                            def_range: to_core_range(assign.range),
                        });
                    }
                }
            }
            Stmt::AnnAssign(ann) => {
                if let Expr::Name(n) = ann.target.as_ref() {
                    snapshot.definitions.push(DefinedSymbol {
                        symbol_fqn: format!("{}.{}", env.module_name, n.id),
                        def_range: to_core_range(ann.range),
                    });
                }
            }
            _ => {}
        }
    }
}

fn build_callable_signature(
    func: &ruff_python_ast::StmtFunctionDef,
    env: &ScopeEnv,
) -> CanonicalType {
    let mut params = Vec::new();
    for p in func
        .parameters
        .posonlyargs
        .iter()
        .chain(func.parameters.args.iter())
        .chain(func.parameters.kwonlyargs.iter())
    {
        let param_type = p
            .parameter
            .annotation
            .as_deref()
            .map(|ann| parse_annotation_expr(ann, env))
            .unwrap_or(CanonicalType::Unknown);
        params.push(ParamInfo {
            name: p.parameter.name.id.to_string(),
            param_type,
            required: p.default.is_none(),
        });
    }
    let return_type = func
        .returns
        .as_deref()
        .map(|r| parse_annotation_expr(r, env))
        .unwrap_or(CanonicalType::Unknown);

    CanonicalType::Callable {
        params,
        has_var_args: func.parameters.vararg.is_some(),
        has_var_kwargs: func.parameters.kwarg.is_some(),
        return_type: Box::new(return_type),
        is_async: func.is_async,
    }
}

fn build_method_signature(
    func: &ruff_python_ast::StmtFunctionDef,
    env: &ScopeEnv,
) -> CanonicalType {
    let mut sig = build_callable_signature(func, env);
    if let CanonicalType::Callable { params, .. } = &mut sig
        && params
            .first()
            .is_some_and(|p| p.name == "self" || p.name == "cls")
    {
        params.remove(0);
    }
    sig
}

/// R24: Intra-Class Return-Type Inference.
/// When a method's explicit return annotation is missing or `Any`, inspect `return <Expr>`
/// statements in the method body and resolve `self.<attr>(...)` where `<attr> = SomeClass`
/// is assigned on `self`'s class or MRO. Returns `None` if no returns exist or if returns conflict.
fn infer_method_return_type_from_body(
    body: &[Stmt],
    cls_fqn: &str,
    class_attrs: &HashMap<String, CanonicalType>,
    direct_bases: &[String],
    bases_map: &HashMap<String, Vec<String>>,
    exports_map: &HashMap<String, CanonicalType>,
) -> Option<CanonicalType> {
    let mut return_exprs: Vec<&Expr> = Vec::new();
    collect_return_exprs(body, &mut return_exprs);
    if return_exprs.is_empty() {
        return None;
    }

    let mut unified: Option<CanonicalType> = None;
    for expr in return_exprs {
        let ty = resolve_intra_class_return_expr(
            expr,
            cls_fqn,
            class_attrs,
            direct_bases,
            bases_map,
            exports_map,
        )?;
        if matches!(ty, CanonicalType::Unknown | CanonicalType::Any) {
            return None;
        }
        match &unified {
            None => unified = Some(ty),
            Some(prev) if prev == &ty => {}
            Some(_) => return None,
        }
    }
    unified
}

fn collect_return_exprs<'a>(stmts: &'a [Stmt], out: &mut Vec<&'a Expr>) {
    for stmt in stmts {
        match stmt {
            Stmt::Return(ret) => {
                if let Some(val) = ret.value.as_deref() {
                    out.push(val);
                }
            }
            Stmt::If(if_stmt) => {
                collect_return_exprs(&if_stmt.body, out);
                for clause in &if_stmt.elif_else_clauses {
                    collect_return_exprs(&clause.body, out);
                }
            }
            Stmt::For(f) => {
                collect_return_exprs(&f.body, out);
                collect_return_exprs(&f.orelse, out);
            }
            Stmt::While(w) => {
                collect_return_exprs(&w.body, out);
                collect_return_exprs(&w.orelse, out);
            }
            Stmt::With(w) => collect_return_exprs(&w.body, out),
            Stmt::Try(t) => {
                collect_return_exprs(&t.body, out);
                for h in &t.handlers {
                    let ruff_python_ast::ExceptHandler::ExceptHandler(eh) = h;
                    collect_return_exprs(&eh.body, out);
                }
                collect_return_exprs(&t.orelse, out);
                collect_return_exprs(&t.finalbody, out);
            }
            _ => {}
        }
    }
}

fn resolve_intra_class_return_expr(
    expr: &Expr,
    cls_fqn: &str,
    class_attrs: &HashMap<String, CanonicalType>,
    direct_bases: &[String],
    bases_map: &HashMap<String, Vec<String>>,
    exports_map: &HashMap<String, CanonicalType>,
) -> Option<CanonicalType> {
    let Expr::Call(call) = expr else {
        return None;
    };
    let Expr::Attribute(attr) = call.func.as_ref() else {
        return None;
    };
    let Expr::Name(recv) = attr.value.as_ref() else {
        return None;
    };
    if recv.id.as_str() != "self" {
        return None;
    }
    let member_name = attr.attr.as_str();

    if let Some(ty) = class_attrs.get(member_name) {
        return Some(ty.clone());
    }

    // Search `cls_fqn` and its MRO in `exports_map`
    let mut queue: Vec<String> = vec![cls_fqn.to_string()];
    queue.extend(direct_bases.iter().cloned());
    let mut visited: HashSet<String> = HashSet::new();
    while let Some(cur) = queue.pop() {
        if !visited.insert(cur.clone()) {
            continue;
        }
        let key = format!("{cur}.{member_name}");
        if let Some(ty) = exports_map.get(&key) {
            return match ty {
                CanonicalType::Callable { return_type, .. } => Some((**return_type).clone()),
                other => Some(other.clone()),
            };
        }
        if let Some(bases) = bases_map.get(&cur) {
            queue.extend(bases.iter().cloned());
        }
    }
    None
}

fn has_transitive_base(
    bases_map: &HashMap<String, Vec<String>>,
    sub_fqn: &str,
    sup_fqn: &str,
) -> bool {
    let mut stack: Vec<&str> = vec![sub_fqn];
    let mut visited: HashSet<&str> = HashSet::new();
    while let Some(cur) = stack.pop() {
        if !visited.insert(cur) {
            continue;
        }
        if let Some(parents) = bases_map.get(cur) {
            for p in parents {
                if p == sup_fqn {
                    return true;
                }
                stack.push(p.as_str());
            }
        }
    }
    false
}

fn parse_annotation_expr(expr: &Expr, env: &ScopeEnv) -> CanonicalType {
    match expr {
        Expr::NoneLiteral(_) => CanonicalType::NoneType,
        Expr::BooleanLiteral(_) => CanonicalType::Primitive(PrimitiveKind::Bool),
        Expr::StringLiteral(_) => CanonicalType::Primitive(PrimitiveKind::Str),
        Expr::BytesLiteral(_) => CanonicalType::Primitive(PrimitiveKind::Bytes),
        Expr::NumberLiteral(num) => match &num.value {
            ruff_python_ast::Number::Int(_) => CanonicalType::Primitive(PrimitiveKind::Int),
            ruff_python_ast::Number::Float(_) | ruff_python_ast::Number::Complex { .. } => {
                CanonicalType::Primitive(PrimitiveKind::Float)
            }
        },
        Expr::Name(n) => parse_named_type(n.id.as_str(), env),
        Expr::Attribute(_) => dotted_expr_name(expr)
            .map(|d| parse_named_type(&d, env))
            .unwrap_or(CanonicalType::Unknown),
        Expr::Subscript(sub) => {
            let head = dotted_expr_name(&sub.value).unwrap_or_default();
            let resolved_head = resolve_imported_dotted(&head, env).unwrap_or(head);
            let is_literal = matches!(
                resolved_head.as_str(),
                "Literal" | "typing.Literal" | "typing_extensions.Literal"
            );
            if is_tuple_type_head(&resolved_head) {
                if let Expr::Tuple(tup) = sub.slice.as_ref() {
                    if tup.elts.len() == 2 && matches!(&tup.elts[1], Expr::EllipsisLiteral(_)) {
                        let elem = parse_annotation_arg_expr(&tup.elts[0], env, false);
                        return CanonicalType::VariadicTuple(Box::new(elem));
                    }
                    if tup.elts.is_empty() {
                        return CanonicalType::Nominal {
                            fqname: "builtins.tuple".to_string(),
                            args: Vec::new(),
                        };
                    }
                } else if matches!(sub.slice.as_ref(), Expr::EllipsisLiteral(_)) {
                    return CanonicalType::Nominal {
                        fqname: "builtins.tuple".to_string(),
                        args: Vec::new(),
                    };
                }
            }
            let args = if let Expr::Tuple(tup) = sub.slice.as_ref() {
                tup.elts
                    .iter()
                    .filter(|e| !matches!(e, Expr::EllipsisLiteral(_)))
                    .map(|e| parse_annotation_arg_expr(e, env, is_literal))
                    .collect()
            } else {
                vec![parse_annotation_arg_expr(&sub.slice, env, is_literal)]
            };
            normalize_subscript_type(&resolved_head, args)
        }
        Expr::BinOp(bin) if matches!(bin.op, ruff_python_ast::Operator::BitOr) => {
            let mut items = Vec::new();
            collect_bitor_union(&bin.left, env, &mut items);
            collect_bitor_union(&bin.right, env, &mut items);
            CanonicalType::Union(items)
        }
        _ => CanonicalType::Unknown,
    }
}

fn parse_annotation_arg_expr(expr: &Expr, env: &ScopeEnv, inside_literal: bool) -> CanonicalType {
    if inside_literal && let Expr::Attribute(attr) = expr {
        // `Literal[Rule.BP001]` -> resolve `Rule` enum class type (`R18`)
        return parse_annotation_expr(&attr.value, env);
    }
    parse_annotation_expr(expr, env)
}

fn collect_bitor_union(expr: &Expr, env: &ScopeEnv, out: &mut Vec<CanonicalType>) {
    if let Expr::BinOp(bin) = expr
        && matches!(bin.op, ruff_python_ast::Operator::BitOr)
    {
        collect_bitor_union(&bin.left, env, out);
        collect_bitor_union(&bin.right, env, out);
        return;
    }
    out.push(parse_annotation_expr(expr, env));
}

fn normalize_subscript_type(head: &str, args: Vec<CanonicalType>) -> CanonicalType {
    match head {
        "Literal" | "typing.Literal" | "typing_extensions.Literal" => collapse_element_types(args),
        other => normalize_subscript_canonical_type(other, args),
    }
}

fn parse_named_type(raw: &str, env: &ScopeEnv) -> CanonicalType {
    if let Some(mapped) = env.imports.get(raw) {
        return parse_type_string_token(&canonicalize_fqn(mapped));
    }
    if let Some(dotted_mapped) = resolve_imported_dotted(raw, env) {
        return parse_type_string_token(&canonicalize_fqn(&dotted_mapped));
    }
    if let Some(CanonicalType::Nominal { fqname, .. }) = env.vars.get(raw) {
        return CanonicalType::Nominal {
            fqname: fqname.clone(),
            args: Vec::new(),
        };
    }
    parse_type_string_token(&canonicalize_fqn(raw))
}

fn parse_type_string_token(raw: &str) -> CanonicalType {
    let trimmed = strip_balanced_outer_parens(raw);
    if trimmed.is_empty() || trimmed == "Unknown" || trimmed.starts_with('@') {
        return CanonicalType::Unknown;
    }
    if matches!(trimmed, "Any" | "typing.Any" | "typing_extensions.Any") {
        return CanonicalType::Any;
    }
    if matches!(trimmed, "None" | "NoneType" | "types.NoneType") {
        return CanonicalType::NoneType;
    }
    // Handle `<class 'int'>` / `<class 'pkg.Cls'>` emitted by `ty` for class literals
    if let Some(cls_inner) = trimmed
        .strip_prefix("<class '")
        .and_then(|s| s.strip_suffix("'>"))
    {
        let target_ty = parse_type_string_token(cls_inner);
        return CanonicalType::Nominal {
            fqname: "builtins.type".to_string(),
            args: vec![target_ty],
        };
    }
    // Handle top-level `A | B` from `ty` display output
    if let Some(parts) = split_top_level_type_pipe(trimmed) {
        let parsed_parts: Vec<CanonicalType> =
            parts.into_iter().map(parse_type_string_token).collect();
        return CanonicalType::Union(parsed_parts);
    }
    // Handle top-level `A & ~B` / `Any & str` intersection refinements from `ty`
    if let Some(conjuncts) = split_top_level_amp(trimmed) {
        let mut positive_concrete: Vec<CanonicalType> = Vec::new();
        let mut saw_any = false;
        for part in conjuncts {
            let p = part.trim();
            if p.starts_with('~') {
                continue;
            }
            let parsed = parse_type_string_token(p);
            match parsed {
                CanonicalType::Any => saw_any = true,
                CanonicalType::Unknown => return CanonicalType::Unknown,
                concrete => positive_concrete.push(concrete),
            }
        }
        if let Some(first_concrete) = positive_concrete.into_iter().next() {
            return first_concrete;
        }
        return if saw_any {
            CanonicalType::Any
        } else {
            CanonicalType::Unknown
        };
    }
    // Handle `Literal[...]` (single/multi-value primitives and `Literal[EnumCls.MEMBER]`)
    if let Some(lit_inner) = trimmed
        .strip_prefix("Literal[")
        .or_else(|| trimmed.strip_prefix("typing.Literal["))
        .or_else(|| trimmed.strip_prefix("typing_extensions.Literal["))
        .and_then(|s| s.strip_suffix(']'))
    {
        let items: Vec<CanonicalType> = split_top_level_type_commas(lit_inner)
            .into_iter()
            .filter(|seg| *seg != "...")
            .map(parse_single_literal_token)
            .collect();
        return collapse_element_types(items);
    }
    // Handle generic `Name[Arg1, Arg2]` emitted by `ty` (preserving variadic `tuple[T, ...]`)
    if let Some(bracket_idx) = trimmed.find('[')
        && trimmed.ends_with(']')
    {
        let head = trimmed[..bracket_idx].trim();
        let inner = &trimmed[bracket_idx + 1..trimmed.len() - 1];
        let raw_args = split_top_level_type_commas(inner);
        if is_tuple_type_head(head) {
            if raw_args.len() == 2 && raw_args[1] == "..." {
                return CanonicalType::VariadicTuple(Box::new(parse_type_string_token(
                    raw_args[0],
                )));
            }
            if raw_args.is_empty() || (raw_args.len() == 1 && matches!(raw_args[0], "()" | "...")) {
                return CanonicalType::Nominal {
                    fqname: "builtins.tuple".to_string(),
                    args: Vec::new(),
                };
            }
        }
        let args = raw_args
            .into_iter()
            .filter(|seg| *seg != "...")
            .map(parse_type_string_token)
            .collect();
        return normalize_subscript_type(head, args);
    }

    parse_bare_nominal_or_primitive(trimmed)
}

fn parse_single_literal_token(raw_lit: &str) -> CanonicalType {
    let lit = raw_lit.trim();
    if lit == "True" || lit == "False" {
        return CanonicalType::Primitive(PrimitiveKind::Bool);
    }
    if lit == "None" {
        return CanonicalType::NoneType;
    }
    if lit.starts_with('"') || lit.starts_with('\'') {
        return CanonicalType::Primitive(PrimitiveKind::Str);
    }
    if lit.starts_with("b\"") || lit.starts_with("b'") {
        return CanonicalType::Primitive(PrimitiveKind::Bytes);
    }
    if lit.parse::<i64>().is_ok() {
        return CanonicalType::Primitive(PrimitiveKind::Int);
    }
    // Enum member literal `EnumCls.MEMBER` or `pkg.EnumCls.MEMBER` (`R18`)
    if let Some((enum_cls, _member)) = lit.rsplit_once('.')
        && !enum_cls.is_empty()
    {
        return parse_type_string_token(enum_cls);
    }
    CanonicalType::Unknown
}

fn split_top_level_amp(s: &str) -> Option<Vec<&str>> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut in_quote: Option<u8> = None;
    let bytes = s.as_bytes();
    let mut last = 0usize;
    let mut idx = 0usize;
    while idx < bytes.len() {
        let b = bytes[idx];
        if let Some(q) = in_quote {
            if b == b'\\' {
                idx += 2;
                continue;
            }
            if b == q {
                in_quote = None;
            }
            idx += 1;
            continue;
        }
        match b {
            b'\'' | b'"' => in_quote = Some(b),
            b'[' | b'(' | b'{' => depth += 1,
            b']' | b')' | b'}' => depth -= 1,
            b'&' if depth == 0 => {
                parts.push(s[last..idx].trim());
                last = idx + 1;
            }
            _ => {}
        }
        idx += 1;
    }
    if parts.is_empty() {
        None
    } else {
        parts.push(s[last..].trim());
        Some(parts)
    }
}

fn convert_salsa_ty_type<'db>(
    ty: TyType<'db>,
    db: &'db RifftySalsaDb,
    prog_env: &ProgramEnvironment<'db>,
) -> CanonicalType {
    match ty {
        TyType::Dynamic(_) | TyType::Divergent(_) => CanonicalType::Unknown,
        TyType::Never => CanonicalType::Unknown,
        ty if ty.is_none(db) => CanonicalType::NoneType,
        TyType::AlwaysTruthy | TyType::AlwaysFalsy => CanonicalType::Primitive(PrimitiveKind::Bool),
        TyType::FunctionLiteral(_) | TyType::BoundMethod(_) | TyType::Callable(_) => {
            // Specific parameter/vararg/kwarg/async flags are resolved via AST/export table,
            // while Salsa confirms callable classification and typeshed return types.
            CanonicalType::Unknown
        }
        other => {
            let rendered = other.display(db, prog_env).to_string();
            if rendered.contains("Unknown") {
                CanonicalType::Unknown
            } else if rendered == "Any" {
                CanonicalType::Any
            } else {
                parse_type_string_token(&rendered)
            }
        }
    }
}

struct TyModelCtx<'a, 'db> {
    db: &'a RifftySalsaDb,
    prog_env: &'a ProgramEnvironment<'db>,
    model: &'a SemanticModel<'db>,
}

fn walk_suite_with_ty<'db>(
    stmts: &[Stmt],
    db: &'db RifftySalsaDb,
    prog_env: &ProgramEnvironment<'db>,
    model: &SemanticModel<'db>,
    env: &mut ScopeEnv,
    snapshot: &mut FileSemanticSnapshot,
) -> bool {
    let ctx = TyModelCtx {
        db,
        prog_env,
        model,
    };
    walk_suite_impl(stmts, Some(&ctx), env, snapshot)
}

fn walk_suite_ast_only(
    stmts: &[Stmt],
    env: &mut ScopeEnv,
    snapshot: &mut FileSemanticSnapshot,
) -> bool {
    walk_suite_impl(stmts, None, env, snapshot)
}

fn walk_suite_impl<'db>(
    stmts: &[Stmt],
    ty_ctx: Option<&TyModelCtx<'_, 'db>>,
    env: &mut ScopeEnv,
    snapshot: &mut FileSemanticSnapshot,
) -> bool {
    let mut terminated = false;
    for stmt in stmts {
        match stmt {
            Stmt::FunctionDef(func) => {
                let sig = build_callable_signature(func, env);
                env.functions.insert(func.name.id.to_string(), sig.clone());
                env.vars.insert(func.name.id.to_string(), sig);

                let mut fn_env = env.clone();
                fn_env.non_none_narrowed.clear();
                for p in func
                    .parameters
                    .posonlyargs
                    .iter()
                    .chain(func.parameters.args.iter())
                    .chain(func.parameters.kwonlyargs.iter())
                {
                    let mut p_ty = p
                        .parameter
                        .annotation
                        .as_deref()
                        .map(|ann| parse_annotation_expr(ann, &fn_env))
                        .unwrap_or(CanonicalType::Unknown);
                    if let Some(def) = &p.default {
                        let def_ty = walk_expr_impl(def, ty_ctx, &fn_env, snapshot);
                        if matches!(def_ty, CanonicalType::NoneType)
                            && !p_ty.is_unknown_or_any()
                            && !p_ty.includes_none()
                        {
                            p_ty = CanonicalType::Union(vec![p_ty, CanonicalType::NoneType]);
                        }
                    }
                    let pname = p.parameter.name.id.to_string();
                    if (pname == "self" || pname == "cls")
                        && matches!(p_ty, CanonicalType::Unknown)
                        && let Some(ref cls_fqn) = fn_env.enclosing_class_fqn
                    {
                        p_ty = CanonicalType::Nominal {
                            fqname: cls_fqn.clone(),
                            args: Vec::new(),
                        };
                    }
                    fn_env.imports.remove(&pname);
                    fn_env.vars.insert(pname, p_ty);
                }
                walk_suite_impl(&func.body, ty_ctx, &mut fn_env, snapshot);
            }
            Stmt::ClassDef(cls) => {
                for base in cls.bases() {
                    walk_expr_impl(base, ty_ctx, env, snapshot);
                }
                let cls_fqn = match &env.enclosing_class_fqn {
                    Some(parent) => format!("{parent}.{}", cls.name.id),
                    None => format!("{}.{}", env.module_name, cls.name.id),
                };
                let cls_ty = CanonicalType::Nominal {
                    fqname: cls_fqn.clone(),
                    args: Vec::new(),
                };
                snapshot
                    .expr_types
                    .insert(to_core_range(cls.range), cls_ty.clone());
                snapshot
                    .expr_types
                    .insert(to_core_range(cls.name.range), cls_ty);
                let mut cls_env = env.clone();
                cls_env.enclosing_class_fqn = Some(cls_fqn);
                walk_suite_impl(&cls.body, ty_ctx, &mut cls_env, snapshot);
            }
            Stmt::Import(imp) => {
                for alias in &imp.names {
                    let full = alias.name.id.to_string();
                    let bound = alias
                        .asname
                        .as_ref()
                        .map(|a| a.id.to_string())
                        .unwrap_or_else(|| full.split('.').next().unwrap_or(&full).to_string());
                    env.imports.insert(bound, full);
                }
            }
            Stmt::ImportFrom(imp) => {
                let base = resolve_import_from_base(imp, &env.module_name);
                for alias in &imp.names {
                    let sym = alias.name.id.as_str();
                    let bound = alias.asname.as_ref().map(|a| a.id.as_str()).unwrap_or(sym);
                    let fqn = if base.is_empty() {
                        sym.to_string()
                    } else {
                        chase_reexport(&env.reexports, &canonicalize_fqn(&format!("{base}.{sym}")))
                    };
                    env.imports.insert(bound.to_string(), fqn.clone());
                    snapshot.symbol_usages.push((fqn, to_core_range(imp.range)));
                }
            }
            Stmt::AnnAssign(ann) => {
                let declared_ty = parse_annotation_expr(&ann.annotation, env);
                if let Some(val) = &ann.value {
                    walk_expr_impl(val, ty_ctx, env, snapshot);
                }
                if let Expr::Name(target) = ann.target.as_ref() {
                    let var_name = target.id.to_string();
                    snapshot
                        .expr_types
                        .insert(to_core_range(target.range), declared_ty.clone());
                    if let Some(val) = &ann.value
                        && !matches!(val.as_ref(), Expr::NoneLiteral(_))
                    {
                        let rhs_ty = snapshot
                            .expr_types
                            .get(&to_core_range(val.range()))
                            .cloned()
                            .unwrap_or(CanonicalType::Unknown);
                        if !rhs_ty.includes_none() && !rhs_ty.is_unknown_or_any() {
                            env.non_none_narrowed.insert(var_name.clone());
                        } else {
                            env.non_none_narrowed.remove(&var_name);
                        }
                    } else {
                        env.non_none_narrowed.remove(&var_name);
                    }
                    env.imports.remove(&var_name);
                    if !matches!(declared_ty, CanonicalType::Callable { .. }) {
                        env.functions.remove(&var_name);
                    }
                    env.vars.insert(var_name, declared_ty);
                }
            }
            Stmt::Assign(assign) => {
                let rhs_ty = walk_expr_impl(&assign.value, ty_ctx, env, snapshot);
                for target in &assign.targets {
                    if let Expr::Name(name) = target {
                        let var_name = name.id.to_string();
                        env.imports.remove(&var_name);
                        if !matches!(rhs_ty, CanonicalType::Callable { .. })
                            && env.functions.remove(&var_name).is_some()
                            && matches!(rhs_ty, CanonicalType::Unknown)
                        {
                            env.vars.insert(var_name.clone(), CanonicalType::Unknown);
                        }
                        if rhs_ty.includes_none() {
                            env.non_none_narrowed.remove(&var_name);
                        } else if !rhs_ty.is_unknown_or_any() {
                            env.non_none_narrowed.insert(var_name.clone());
                        }
                        if !matches!(rhs_ty, CanonicalType::Unknown) {
                            let keep_union = env
                                .vars
                                .get(&var_name)
                                .is_some_and(CanonicalType::includes_none)
                                && !rhs_ty.includes_none();
                            if !keep_union {
                                env.vars.insert(var_name.clone(), rhs_ty.clone());
                            }
                        }
                        snapshot
                            .expr_types
                            .insert(to_core_range(name.range), rhs_ty.clone());
                    } else {
                        walk_expr_impl(target, ty_ctx, env, snapshot);
                    }
                }
            }
            Stmt::If(if_stmt) => {
                if is_type_checking_guard(&if_stmt.test, env) {
                    collect_top_level_imports(&if_stmt.body, env);
                }
                walk_expr_impl(&if_stmt.test, ty_ctx, env, snapshot);
                let (non_none_then, none_then) = extract_none_guards(&if_stmt.test);

                let mut then_env = env.clone();
                for v in &non_none_then {
                    then_env.non_none_narrowed.insert(v.clone());
                }
                let then_term = walk_suite_impl(&if_stmt.body, ty_ctx, &mut then_env, snapshot);

                let mut else_env = env.clone();
                for v in &none_then {
                    else_env.non_none_narrowed.insert(v.clone());
                }
                for clause in &if_stmt.elif_else_clauses {
                    let mut clause_env = else_env.clone();
                    if let Some(t) = &clause.test {
                        walk_expr_impl(t, ty_ctx, &clause_env, snapshot);
                        let (nn, _) = extract_none_guards(t);
                        for v in nn {
                            clause_env.non_none_narrowed.insert(v);
                        }
                    }
                    walk_suite_impl(&clause.body, ty_ctx, &mut clause_env, snapshot);
                }

                if then_term && if_stmt.elif_else_clauses.is_empty() {
                    for v in none_then {
                        env.non_none_narrowed.insert(v);
                    }
                }
            }
            Stmt::Assert(assert_stmt) => {
                walk_expr_impl(&assert_stmt.test, ty_ctx, env, snapshot);
                let (non_none, _) = extract_none_guards(&assert_stmt.test);
                for v in non_none {
                    env.non_none_narrowed.insert(v);
                }
                if let Some(msg) = &assert_stmt.msg {
                    walk_expr_impl(msg, ty_ctx, env, snapshot);
                }
            }
            Stmt::Return(ret) => {
                if let Some(val) = &ret.value {
                    walk_expr_impl(val, ty_ctx, env, snapshot);
                }
                terminated = true;
            }
            Stmt::Raise(raise_stmt) => {
                if let Some(exc) = &raise_stmt.exc {
                    walk_expr_impl(exc, ty_ctx, env, snapshot);
                }
                terminated = true;
            }
            Stmt::Expr(expr_stmt) => {
                walk_expr_impl(&expr_stmt.value, ty_ctx, env, snapshot);
            }
            Stmt::For(for_stmt) => {
                let iter_ty = walk_expr_impl(&for_stmt.iter, ty_ctx, env, snapshot);
                let mut loop_env = env.clone();
                if let Expr::Name(target) = for_stmt.target.as_ref()
                    && let Some(elem_ty) = iter_ty.container_element_type()
                {
                    loop_env.vars.insert(target.id.to_string(), elem_ty);
                }
                walk_suite_impl(&for_stmt.body, ty_ctx, &mut loop_env, snapshot);
                walk_suite_impl(&for_stmt.orelse, ty_ctx, env, snapshot);
            }
            Stmt::While(while_stmt) => {
                walk_expr_impl(&while_stmt.test, ty_ctx, env, snapshot);
                let mut loop_env = env.clone();
                walk_suite_impl(&while_stmt.body, ty_ctx, &mut loop_env, snapshot);
            }
            Stmt::With(with_stmt) => {
                for item in &with_stmt.items {
                    walk_expr_impl(&item.context_expr, ty_ctx, env, snapshot);
                }
                walk_suite_impl(&with_stmt.body, ty_ctx, env, snapshot);
            }
            Stmt::Try(try_stmt) => {
                let mut try_env = env.clone();
                walk_suite_impl(&try_stmt.body, ty_ctx, &mut try_env, snapshot);
                for h in &try_stmt.handlers {
                    let ruff_python_ast::ExceptHandler::ExceptHandler(eh) = h;
                    let mut eh_env = env.clone();
                    walk_suite_impl(&eh.body, ty_ctx, &mut eh_env, snapshot);
                }
                walk_suite_impl(&try_stmt.orelse, ty_ctx, env, snapshot);
                walk_suite_impl(&try_stmt.finalbody, ty_ctx, env, snapshot);
            }
            _ => {}
        }
    }
    terminated
}

fn walk_expr_impl<'db>(
    expr: &Expr,
    ty_ctx: Option<&TyModelCtx<'_, 'db>>,
    env: &ScopeEnv,
    snapshot: &mut FileSemanticSnapshot,
) -> CanonicalType {
    let range = to_core_range(expr.range());
    let salsa_hint = ty_ctx
        .and_then(|ctx| ExprRef::from(expr).inferred_type(ctx.model))
        .map(|t| {
            let ctx = ty_ctx.expect("checked Some");
            convert_salsa_ty_type(t, ctx.db, ctx.prog_env)
        });

    let (mut ty, purity, mut nullability) = match expr {
        Expr::NoneLiteral(_) => (CanonicalType::NoneType, Purity::Pure, TriState::Yes),
        Expr::BooleanLiteral(_) => (
            CanonicalType::Primitive(PrimitiveKind::Bool),
            Purity::Pure,
            TriState::No,
        ),
        Expr::NumberLiteral(num) => {
            let prim = match &num.value {
                ruff_python_ast::Number::Int(_) => PrimitiveKind::Int,
                ruff_python_ast::Number::Float(_) | ruff_python_ast::Number::Complex { .. } => {
                    PrimitiveKind::Float
                }
            };
            (CanonicalType::Primitive(prim), Purity::Pure, TriState::No)
        }
        Expr::StringLiteral(_) => (
            CanonicalType::Primitive(PrimitiveKind::Str),
            Purity::Pure,
            TriState::No,
        ),
        Expr::FString(fs) => {
            let mut fp = Purity::Pure;
            for f in fs.value.f_strings() {
                for el in &f.elements {
                    if let ruff_python_ast::InterpolatedStringElement::Interpolation(it) = el {
                        walk_expr_impl(&it.expression, ty_ctx, env, snapshot);
                        let sub_p = snapshot
                            .expr_purity
                            .get(&to_core_range(it.expression.range()))
                            .copied()
                            .unwrap_or(Purity::SideEffecting);
                        fp = fp.combine(sub_p);
                    }
                }
            }
            (
                CanonicalType::Primitive(PrimitiveKind::Str),
                fp,
                TriState::No,
            )
        }
        Expr::BytesLiteral(_) => (
            CanonicalType::Primitive(PrimitiveKind::Bytes),
            Purity::Pure,
            TriState::No,
        ),
        Expr::Name(name) => {
            let id = name.id.as_str();
            if (id == "self" || id == "cls") && env.enclosing_class_fqn.is_some() {
                snapshot.self_or_cls_exprs.insert(range);
            }
            if let Some(fqn) = env.imports.get(id) {
                snapshot.expr_fqnames.insert(range, fqn.clone());
                snapshot.symbol_usages.push((fqn.clone(), range));
                let resolved_ty = env
                    .workspace_exports
                    .get(fqn)
                    .cloned()
                    .or_else(|| known_ecosystem_callable(fqn))
                    .unwrap_or(CanonicalType::Unknown);
                (resolved_ty, Purity::Pure, TriState::No)
            } else if let Some(var_ty) = env.vars.get(id).cloned() {
                if let Some(fn_ty) = env.functions.get(id)
                    && matches!(fn_ty, CanonicalType::Callable { .. })
                {
                    let fqn = format!("{}.{}", env.module_name, id);
                    snapshot.expr_fqnames.insert(range, id.to_string());
                    snapshot.symbol_usages.push((fqn, range));
                }
                if env.non_none_narrowed.contains(id) {
                    (var_ty.strip_none(), Purity::Pure, TriState::No)
                } else {
                    let nul = if var_ty.is_unknown_or_any() {
                        TriState::Unknown
                    } else if var_ty.includes_none() {
                        TriState::Yes
                    } else {
                        TriState::No
                    };
                    (var_ty, Purity::Pure, nul)
                }
            } else {
                if is_pure_builtin(id) {
                    snapshot
                        .expr_fqnames
                        .insert(range, format!("builtins.{id}"));
                }
                (CanonicalType::Unknown, Purity::Pure, TriState::Unknown)
            }
        }
        Expr::Attribute(attr) => {
            let recv_ty = walk_expr_impl(&attr.value, ty_ctx, env, snapshot);
            let recv_purity = snapshot
                .expr_purity
                .get(&to_core_range(attr.value.range()))
                .copied()
                .unwrap_or(Purity::Pure);

            let mut attr_ty = CanonicalType::Unknown;
            if let Some(dotted) = dotted_expr_name(expr)
                && let Some(resolved) = resolve_imported_dotted(&dotted, env)
            {
                let canonical = canonicalize_fqn(&resolved);
                let (mro_fqn, mro_ty) = resolve_mro_member(&canonical, env);
                snapshot.expr_fqnames.insert(range, mro_fqn.clone());
                if mro_fqn != canonical {
                    snapshot.symbol_usages.push((canonical, range));
                }
                snapshot.symbol_usages.push((mro_fqn, range));
                attr_ty = mro_ty;
            } else if let CanonicalType::Nominal { fqname, .. } = &recv_ty {
                let candidate = canonicalize_fqn(&format!("{fqname}.{}", attr.attr.id));
                let (mro_fqn, mut mro_ty) = resolve_mro_member(&candidate, env);
                if matches!(mro_ty, CanonicalType::Unknown) && attr.attr.id.as_str() == "copy" {
                    mro_ty = CanonicalType::Callable {
                        params: Vec::new(),
                        has_var_args: true,
                        has_var_kwargs: true,
                        return_type: Box::new(recv_ty.clone()),
                        is_async: false,
                    };
                }
                snapshot.expr_fqnames.insert(range, mro_fqn.clone());
                snapshot.symbol_usages.push((mro_fqn, range));
                attr_ty = mro_ty;
            }
            (attr_ty, recv_purity, TriState::Unknown)
        }
        Expr::Call(call) => {
            let callee_ty = walk_expr_impl(&call.func, ty_ctx, env, snapshot);
            let callee_range = to_core_range(call.func.range());
            let callee_fqn = snapshot.expr_fqnames.get(&callee_range).cloned();
            if let Some(ref fqn) = callee_fqn {
                snapshot.symbol_usages.push((fqn.clone(), range));
            }

            let mut args_purity = Purity::Pure;
            for arg in &call.arguments.args {
                walk_expr_impl(arg, ty_ctx, env, snapshot);
                let ap = snapshot
                    .expr_purity
                    .get(&to_core_range(arg.range()))
                    .copied()
                    .unwrap_or(Purity::SideEffecting);
                args_purity = args_purity.combine(ap);
            }
            for kw in &call.arguments.keywords {
                walk_expr_impl(&kw.value, ty_ctx, env, snapshot);
                let kp = snapshot
                    .expr_purity
                    .get(&to_core_range(kw.value.range()))
                    .copied()
                    .unwrap_or(Purity::SideEffecting);
                args_purity = args_purity.combine(kp);
            }

            let call_purity = if callee_fqn.as_deref().is_some_and(is_pure_callable_fqn) {
                args_purity
            } else {
                Purity::SideEffecting
            };

            let result_ty = match callee_ty {
                CanonicalType::Callable {
                    return_type,
                    is_async: true,
                    ..
                } => CanonicalType::Nominal {
                    fqname: "typing.Coroutine".to_string(),
                    args: vec![*return_type],
                },
                CanonicalType::Callable {
                    return_type,
                    is_async: false,
                    ..
                } => *return_type,
                CanonicalType::Nominal { fqname, mut args }
                    if fqname == "builtins.type" && !args.is_empty() =>
                {
                    args.remove(0)
                }
                CanonicalType::Nominal { fqname, args } => CanonicalType::Nominal { fqname, args },
                _ => infer_builtin_collection_call(call, snapshot),
            };
            let nul = if result_ty.is_unknown_or_any() {
                TriState::Unknown
            } else if result_ty.includes_none() {
                TriState::Yes
            } else {
                TriState::No
            };
            (result_ty, call_purity, nul)
        }
        Expr::Await(aw) => {
            let inner = walk_expr_impl(&aw.value, ty_ctx, env, snapshot);
            let unwrapped = match inner {
                CanonicalType::Nominal { fqname, mut args }
                    if fqname == "typing.Coroutine" && !args.is_empty() =>
                {
                    args.remove(0)
                }
                other => other,
            };
            (unwrapped, Purity::SideEffecting, TriState::Unknown)
        }
        Expr::Set(set) => {
            let (elem, p) = infer_homogeneous_elements(&set.elts, ty_ctx, env, snapshot);
            let args = if matches!(elem, CanonicalType::Unknown) {
                Vec::new()
            } else {
                vec![elem]
            };
            (
                CanonicalType::Nominal {
                    fqname: "builtins.set".to_string(),
                    args,
                },
                p,
                TriState::No,
            )
        }
        Expr::List(lst) => {
            let (elem, p) = infer_homogeneous_elements(&lst.elts, ty_ctx, env, snapshot);
            let args = if matches!(elem, CanonicalType::Unknown) {
                Vec::new()
            } else {
                vec![elem]
            };
            (
                CanonicalType::Nominal {
                    fqname: "builtins.list".to_string(),
                    args,
                },
                p,
                TriState::No,
            )
        }
        Expr::Tuple(tup) => {
            let mut p = Purity::Pure;
            let mut args = Vec::with_capacity(tup.elts.len());
            for el in &tup.elts {
                args.push(walk_expr_impl(el, ty_ctx, env, snapshot));
                let ep = snapshot
                    .expr_purity
                    .get(&to_core_range(el.range()))
                    .copied()
                    .unwrap_or(Purity::SideEffecting);
                p = p.combine(ep);
            }
            (
                CanonicalType::Nominal {
                    fqname: "builtins.tuple".to_string(),
                    args,
                },
                p,
                TriState::No,
            )
        }
        Expr::Dict(dict) => {
            let mut p = Purity::Pure;
            let mut key_types = Vec::new();
            let mut val_types = Vec::new();
            for item in &dict.items {
                if let Some(k) = &item.key {
                    key_types.push(walk_expr_impl(k, ty_ctx, env, snapshot));
                    let kp = snapshot
                        .expr_purity
                        .get(&to_core_range(k.range()))
                        .copied()
                        .unwrap_or(Purity::SideEffecting);
                    p = p.combine(kp);
                }
                val_types.push(walk_expr_impl(&item.value, ty_ctx, env, snapshot));
                let vp = snapshot
                    .expr_purity
                    .get(&to_core_range(item.value.range()))
                    .copied()
                    .unwrap_or(Purity::SideEffecting);
                p = p.combine(vp);
            }
            let kt = collapse_element_types(key_types);
            let vt = collapse_element_types(val_types);
            let args = if matches!(kt, CanonicalType::Unknown) {
                Vec::new()
            } else {
                vec![kt, vt]
            };
            (
                CanonicalType::Nominal {
                    fqname: "builtins.dict".to_string(),
                    args,
                },
                p,
                TriState::No,
            )
        }
        Expr::Compare(cmp) => {
            let mut p = Purity::Pure;
            for opnd in &cmp.operands {
                walk_expr_impl(opnd, ty_ctx, env, snapshot);
                let op_p = snapshot
                    .expr_purity
                    .get(&to_core_range(opnd.range()))
                    .copied()
                    .unwrap_or(Purity::SideEffecting);
                p = p.combine(op_p);
            }
            (
                CanonicalType::Primitive(PrimitiveKind::Bool),
                p,
                TriState::No,
            )
        }
        Expr::BinOp(bin) => {
            let lt = walk_expr_impl(&bin.left, ty_ctx, env, snapshot);
            let rt = walk_expr_impl(&bin.right, ty_ctx, env, snapshot);
            let lp = snapshot
                .expr_purity
                .get(&to_core_range(bin.left.range()))
                .copied()
                .unwrap_or(Purity::SideEffecting);
            let rp = snapshot
                .expr_purity
                .get(&to_core_range(bin.right.range()))
                .copied()
                .unwrap_or(Purity::SideEffecting);
            let out_ty = if lt == rt { lt } else { CanonicalType::Unknown };
            (out_ty, lp.combine(rp), TriState::Unknown)
        }
        Expr::Subscript(sub) => {
            let ct = walk_expr_impl(&sub.value, ty_ctx, env, snapshot);
            walk_expr_impl(&sub.slice, ty_ctx, env, snapshot);
            let vp = snapshot
                .expr_purity
                .get(&to_core_range(sub.value.range()))
                .copied()
                .unwrap_or(Purity::SideEffecting);
            let sp = snapshot
                .expr_purity
                .get(&to_core_range(sub.slice.range()))
                .copied()
                .unwrap_or(Purity::SideEffecting);
            let elem_ty = match &ct {
                CanonicalType::Nominal { fqname, .. }
                    if fqname == "pandas.DataFrame"
                        && !matches!(
                            sub.slice.as_ref(),
                            Expr::Compare(_) | Expr::Slice(_) | Expr::List(_) | Expr::Tuple(_)
                        ) =>
                {
                    CanonicalType::Nominal {
                        fqname: "pandas.Series".to_string(),
                        args: Vec::new(),
                    }
                }
                _ => ct.subscript_value_type().unwrap_or(CanonicalType::Unknown),
            };
            (elem_ty, vp.combine(sp), TriState::Unknown)
        }
        Expr::Starred(st) => {
            walk_expr_impl(&st.value, ty_ctx, env, snapshot);
            let sp = snapshot
                .expr_purity
                .get(&to_core_range(st.value.range()))
                .copied()
                .unwrap_or(Purity::SideEffecting);
            (CanonicalType::Unknown, sp, TriState::Unknown)
        }
        _ => (
            CanonicalType::Unknown,
            Purity::SideEffecting,
            TriState::Unknown,
        ),
    };

    // Enrich `Unknown` expressions (and unparameterized builtin collection calls like `tuple(gen)`)
    // with Astral `ty` Salsa `SemanticModel` inferred types!
    let is_unparameterized_collection_call = matches!(expr, Expr::Call(_))
        && matches!(
            &ty,
            CanonicalType::Nominal { fqname, args }
                if args.is_empty()
                    && matches!(
                        fqname.as_str(),
                        "builtins.tuple"
                            | "builtins.list"
                            | "builtins.set"
                            | "builtins.frozenset"
                            | "builtins.dict"
                    )
        );
    if let Some(salsa_ty) = salsa_hint {
        if (matches!(ty, CanonicalType::Unknown) || is_unparameterized_collection_call)
            && !salsa_ty.is_unknown_or_any()
        {
            if matches!(nullability, TriState::Unknown) {
                nullability = if salsa_ty.includes_none() {
                    TriState::Yes
                } else {
                    TriState::No
                };
            }
            ty = salsa_ty;
        } else if matches!(ty, CanonicalType::Unknown) && matches!(salsa_ty, CanonicalType::Any) {
            ty = CanonicalType::Any;
        }
    }

    snapshot.expr_types.insert(range, ty.clone());
    snapshot.expr_purity.insert(range, purity);
    snapshot.expr_nullability.insert(range, nullability);
    ty
}

fn infer_homogeneous_elements<'db>(
    elts: &[Expr],
    ty_ctx: Option<&TyModelCtx<'_, 'db>>,
    env: &ScopeEnv,
    snapshot: &mut FileSemanticSnapshot,
) -> (CanonicalType, Purity) {
    let mut p = Purity::Pure;
    let mut types = Vec::with_capacity(elts.len());
    for el in elts {
        types.push(walk_expr_impl(el, ty_ctx, env, snapshot));
        let ep = snapshot
            .expr_purity
            .get(&to_core_range(el.range()))
            .copied()
            .unwrap_or(Purity::SideEffecting);
        p = p.combine(ep);
    }
    (collapse_element_types(types), p)
}

fn collapse_element_types(types: Vec<CanonicalType>) -> CanonicalType {
    let mut unique: Vec<CanonicalType> = Vec::new();
    for t in types {
        if matches!(t, CanonicalType::Unknown) {
            return CanonicalType::Unknown;
        }
        if !unique.contains(&t) {
            unique.push(t);
        }
    }
    match unique.len() {
        0 => CanonicalType::Unknown,
        1 => unique.remove(0),
        _ => CanonicalType::Union(unique),
    }
}

fn infer_builtin_collection_call(
    call: &ruff_python_ast::ExprCall,
    snapshot: &FileSemanticSnapshot,
) -> CanonicalType {
    let Expr::Name(n) = call.func.as_ref() else {
        return CanonicalType::Unknown;
    };
    match n.id.as_str() {
        "set" | "frozenset" | "list" => {
            let elem = call
                .arguments
                .args
                .first()
                .and_then(|a| snapshot.expr_types.get(&to_core_range(a.range())))
                .and_then(CanonicalType::container_element_type);
            CanonicalType::Nominal {
                fqname: format!("builtins.{}", n.id),
                args: elem.into_iter().collect(),
            }
        }
        "tuple" => {
            let elem = call
                .arguments
                .args
                .first()
                .and_then(|a| snapshot.expr_types.get(&to_core_range(a.range())))
                .and_then(CanonicalType::container_element_type);
            match elem {
                Some(elem_ty) if !matches!(elem_ty, CanonicalType::Unknown) => {
                    CanonicalType::VariadicTuple(Box::new(elem_ty))
                }
                _ => CanonicalType::Nominal {
                    fqname: "builtins.tuple".to_string(),
                    args: Vec::new(),
                },
            }
        }
        "dict" => CanonicalType::Nominal {
            fqname: "builtins.dict".to_string(),
            args: Vec::new(),
        },
        "int" => CanonicalType::Primitive(PrimitiveKind::Int),
        "str" => CanonicalType::Primitive(PrimitiveKind::Str),
        "float" => CanonicalType::Primitive(PrimitiveKind::Float),
        "bool" => CanonicalType::Primitive(PrimitiveKind::Bool),
        "bytes" => CanonicalType::Primitive(PrimitiveKind::Bytes),
        _ => CanonicalType::Unknown,
    }
}

fn extract_none_guards(test: &Expr) -> (Vec<String>, Vec<String>) {
    let mut non_none_then = Vec::new();
    let mut none_then = Vec::new();
    match test {
        Expr::Compare(cmp) if cmp.ops.len() == 1 && cmp.operands.len() == 2 => {
            let lhs = &cmp.operands[0];
            let rhs = &cmp.operands[1];
            match cmp.ops[0] {
                CmpOp::IsNot => {
                    if let (Expr::Name(n), Expr::NoneLiteral(_))
                    | (Expr::NoneLiteral(_), Expr::Name(n)) = (lhs, rhs)
                    {
                        non_none_then.push(n.id.to_string());
                    }
                }
                CmpOp::Is => {
                    if let (Expr::Name(n), Expr::NoneLiteral(_))
                    | (Expr::NoneLiteral(_), Expr::Name(n)) = (lhs, rhs)
                    {
                        none_then.push(n.id.to_string());
                    }
                }
                _ => {}
            }
        }
        Expr::Name(n) => non_none_then.push(n.id.to_string()),
        _ => {}
    }
    (non_none_then, none_then)
}

fn dotted_expr_name(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Name(n) => Some(n.id.to_string()),
        Expr::Attribute(attr) => {
            let prefix = dotted_expr_name(&attr.value)?;
            Some(format!("{prefix}.{}", attr.attr.id))
        }
        _ => None,
    }
}

fn resolve_imported_dotted(dotted: &str, env: &ScopeEnv) -> Option<String> {
    if let Some(exact) = env.imports.get(dotted) {
        return Some(exact.clone());
    }
    let (head, tail) = dotted.split_once('.')?;
    let resolved_head = env.imports.get(head)?;
    Some(format!("{resolved_head}.{tail}"))
}

fn canonicalize_fqn(fqn: &str) -> String {
    match fqn {
        "np.testing.assert_warns" => "numpy.testing.assert_warns".to_string(),
        "pandas.core.frame.DataFrame" => "pandas.DataFrame".to_string(),
        "pandas.core.series.Series" => "pandas.Series".to_string(),
        "sqlalchemy.orm.session.Session" => "sqlalchemy.orm.Session".to_string(),
        "pydantic.main.BaseModel" => "pydantic.BaseModel".to_string(),
        other => other.to_string(),
    }
}

fn resolve_mro_member(candidate_fqn: &str, env: &ScopeEnv) -> (String, CanonicalType) {
    if let Some(ty) = env
        .property_exports
        .get(candidate_fqn)
        .cloned()
        .or_else(|| env.workspace_exports.get(candidate_fqn).cloned())
        .or_else(|| known_ecosystem_callable(candidate_fqn))
    {
        return (candidate_fqn.to_string(), ty);
    }
    if let Some((owner_cls, attr)) = candidate_fqn.rsplit_once('.') {
        let mut queue: std::collections::VecDeque<&str> = std::collections::VecDeque::new();
        let mut visited: HashSet<&str> = HashSet::new();
        queue.push_back(owner_cls);
        while let Some(cur) = queue.pop_front() {
            if !visited.insert(cur) {
                continue;
            }
            if let Some(parents) = env.class_bases.get(cur) {
                for p in parents {
                    let parent_member = format!("{p}.{attr}");
                    if let Some(ty) = env
                        .property_exports
                        .get(&parent_member)
                        .cloned()
                        .or_else(|| env.workspace_exports.get(&parent_member).cloned())
                        .or_else(|| known_ecosystem_callable(&parent_member))
                    {
                        return (parent_member, ty);
                    }
                    if !env.class_bases.contains_key(p) && p.contains('.') {
                        // External base class (e.g. `pydantic.BaseModel`, `sqlalchemy.orm.Session`):
                        return (parent_member, CanonicalType::Unknown);
                    }
                    queue.push_back(p.as_str());
                }
            }
        }
    }
    (candidate_fqn.to_string(), CanonicalType::Unknown)
}

fn known_ecosystem_callable(fqn: &str) -> Option<CanonicalType> {
    match fqn {
        "asyncio.sleep" => Some(CanonicalType::Callable {
            params: vec![ParamInfo {
                name: "delay".to_string(),
                param_type: CanonicalType::Primitive(PrimitiveKind::Float),
                required: true,
            }],
            has_var_args: false,
            has_var_kwargs: false,
            return_type: Box::new(CanonicalType::NoneType),
            is_async: true,
        }),
        "pandas.DataFrame"
        | "pandas.read_csv"
        | "pandas.read_parquet"
        | "pandas.read_json"
        | "pandas.concat" => Some(CanonicalType::Callable {
            params: Vec::new(),
            has_var_args: true,
            has_var_kwargs: true,
            return_type: Box::new(CanonicalType::Nominal {
                fqname: "pandas.DataFrame".to_string(),
                args: Vec::new(),
            }),
            is_async: false,
        }),
        "pandas.Series" => Some(CanonicalType::Callable {
            params: Vec::new(),
            has_var_args: true,
            has_var_kwargs: true,
            return_type: Box::new(CanonicalType::Nominal {
                fqname: "pandas.Series".to_string(),
                args: Vec::new(),
            }),
            is_async: false,
        }),
        _ => None,
    }
}

fn is_pure_builtin(name: &str) -> bool {
    matches!(
        name,
        "len" | "abs" | "min" | "max" | "int" | "str" | "float" | "bool" | "bytes" | "isinstance"
    )
}

fn is_pure_callable_fqn(fqn: &str) -> bool {
    matches!(
        fqn,
        "builtins.len"
            | "builtins.abs"
            | "builtins.min"
            | "builtins.max"
            | "builtins.int"
            | "builtins.str"
            | "builtins.float"
            | "builtins.bool"
            | "builtins.bytes"
            | "builtins.isinstance"
    )
}

#[cfg(test)]
mod tests {
    use riffty_core::are_types_provably_disjoint;

    use super::*;

    fn span_of(source: &str, needle: &str) -> TextRange {
        let idx = source
            .find(needle)
            .unwrap_or_else(|| panic!("needle `{needle}` not found in:\n{source}"));
        TextRange::new(idx as u32, (idx + needle.len()) as u32)
    }

    #[test]
    fn r18_str_enum_and_literal_enum_member_are_subtypes_of_str_and_rule() {
        let src = concat!(
            "from enum import Enum\n",
            "from typing import Literal\n",
            "\n",
            "class Rule(str, Enum):\n",
            "    BP001 = 'BP001'\n",
            "\n",
            "def check(r: Literal[Rule.BP001], disabled: set[Rule | str]) -> bool:\n",
            "    return r in disabled\n",
        );
        let oracle = TyOracleAdapter::new();
        oracle.index_single_file(FileId(1), "bump_pydantic/codemods/__init__.py", src);

        let r_span = span_of(src, "r in disabled");
        let r_only = TextRange::new(r_span.start.0, r_span.start.0 + 1);
        let r_ty = oracle.type_of(FileId(1), r_only);
        assert_eq!(
            oracle.is_subtype(&r_ty, &CanonicalType::Primitive(PrimitiveKind::Str)),
            TriState::Yes,
            "Rule(str, Enum) member must be a subtype of str; got {r_ty:?}"
        );
        assert!(
            !are_types_provably_disjoint(
                &r_ty,
                &CanonicalType::Primitive(PrimitiveKind::Str),
                &oracle
            ),
            "Rule(str, Enum) must never be considered disjoint from str"
        );
    }

    #[test]
    fn r18_property_getter_unwraps_return_type_and_local_var_shadows_module_fn() {
        let src = concat!(
            "from typing import Literal\n",
            "\n",
            "def mode(a: int) -> int:\n",
            "    return a\n",
            "\n",
            "def _unknown_helper(x):\n",
            "    return x\n",
            "\n",
            "class ShortTimeFFT:\n",
            "    @property\n",
            "    def fft_mode(self) -> Literal['twosided', 'centered', 'onesided']:\n",
            "        return 'onesided'\n",
            "\n",
            "    def check_prop(self) -> bool:\n",
            "        return self.fft_mode in {'onesided', 'twosided'}\n",
            "\n",
            "def stats_fn(raw) -> bool:\n",
            "    mode = _unknown_helper(raw)\n",
            "    return mode in ['a', 'b']\n",
        );
        let oracle = TyOracleAdapter::new();
        oracle.index_single_file(FileId(1), "scipy/signal/_short_time_fft.py", src);

        let prop_span = span_of(src, "self.fft_mode");
        let prop_ty = oracle.type_of(FileId(1), prop_span);
        assert_eq!(prop_ty, CanonicalType::Primitive(PrimitiveKind::Str));

        let shadowed_use = span_of(src, "mode in ['a', 'b']");
        let mode_span = TextRange::new(shadowed_use.start.0, shadowed_use.start.0 + 4);
        let mode_ty = oracle.type_of(FileId(1), mode_span);
        assert_eq!(
            mode_ty,
            CanonicalType::Unknown,
            "Local assignment `mode = _unknown_helper(raw)` must shadow module-level `def mode`"
        );
    }

    #[test]
    fn r22_pep562_type_checking_4hop_mro_and_pydantic_v1_separation() {
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
                source: concat!(
                    "from pydantic import BaseModel\n",
                    "from pydantic.v1 import BaseModel as V1BaseModel\n",
                    "\n",
                    "class Serializable(BaseModel):\n",
                    "    pass\n",
                    "\n",
                    "class LegacyV1Model(V1BaseModel):\n",
                    "    pass\n",
                )
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
                    "\n",
                    "class RunnableSerializable(Serializable):\n",
                    "    pass\n",
                )
                .to_string(),
            },
            WorkspaceFile {
                file_id: FileId(6),
                path: PathBuf::from("libs/core/langchain_core/language_models/__init__.py"),
                relative_path: "libs/core/langchain_core/language_models/__init__.py".to_string(),
                source: String::new(),
            },
            WorkspaceFile {
                file_id: FileId(7),
                path: PathBuf::from("libs/core/langchain_core/language_models/base.py"),
                relative_path: "libs/core/langchain_core/language_models/base.py".to_string(),
                source: concat!(
                    "from langchain_core.runnables.base import RunnableSerializable\n",
                    "\n",
                    "class BaseLanguageModel(RunnableSerializable):\n",
                    "    pass\n",
                )
                .to_string(),
            },
            WorkspaceFile {
                file_id: FileId(8),
                path: PathBuf::from("libs/core/langchain_core/language_models/llms.py"),
                relative_path: "libs/core/langchain_core/language_models/llms.py".to_string(),
                source: concat!(
                    "from langchain_core.language_models.base import BaseLanguageModel\n",
                    "\n",
                    "class BaseLLM(BaseLanguageModel):\n",
                    "    def _identifying_params(self):\n",
                    "        return self.dict()\n",
                )
                .to_string(),
            },
        ];

        let oracle = TyOracleAdapter::new();
        oracle.index_workspace(&files);

        let pydantic_v2 = CanonicalType::Nominal {
            fqname: "pydantic.BaseModel".to_string(),
            args: Vec::new(),
        };
        let base_llm = CanonicalType::Nominal {
            fqname: "langchain_core.language_models.llms.BaseLLM".to_string(),
            args: Vec::new(),
        };
        let legacy_v1 = CanonicalType::Nominal {
            fqname: "langchain_core.load.serializable.LegacyV1Model".to_string(),
            args: Vec::new(),
        };

        assert_eq!(oracle.is_subtype(&base_llm, &pydantic_v2), TriState::Yes);
        assert_ne!(oracle.is_subtype(&legacy_v1, &pydantic_v2), TriState::Yes);
    }

    #[test]
    fn r17_method_override_risk_distinguishes_open_self_subclass_override_and_closed_concrete() {
        let src = concat!(
            "from typing import final\n",
            "from pydantic import BaseModel\n",
            "\n",
            "class OpenPrompt(BaseModel):\n",
            "    def compat(self):\n",
            "        return self.dict()\n",
            "\n",
            "class CustomPrompt(OpenPrompt):\n",
            "    def dict(self, **kwargs):\n",
            "        return {'custom': True}\n",
            "\n",
            "class ClosedSpec(BaseModel):\n",
            "    name: str\n",
            "\n",
            "@final\n",
            "class FinalEvent(BaseModel):\n",
            "    def dump_self(self):\n",
            "        return self.dict()\n",
            "\n",
            "def run(p: OpenPrompt, s: ClosedSpec):\n",
            "    a = p.dict()\n",
            "    b = s.dict()\n",
            "    return a, b\n",
        );
        let oracle = TyOracleAdapter::new();
        oracle.index_single_file(FileId(1), "models.py", src);

        let open_self = span_of(src, "self.dict()");
        let open_self_recv = TextRange::new(open_self.start.0, open_self.start.0 + 4);
        assert_eq!(
            oracle.method_override_risk(FileId(1), open_self_recv, "dict", "pydantic.BaseModel"),
            OverrideRisk::PolymorphicOrOverridden
        );

        let p_call = span_of(src, "p.dict()");
        let p_recv = TextRange::new(p_call.start.0, p_call.start.0 + 1);
        assert_eq!(
            oracle.method_override_risk(FileId(1), p_recv, "dict", "pydantic.BaseModel"),
            OverrideRisk::PolymorphicOrOverridden
        );

        let s_call = span_of(src, "s.dict()");
        let s_recv = TextRange::new(s_call.start.0, s_call.start.0 + 1);
        assert_eq!(
            oracle.method_override_risk(FileId(1), s_recv, "dict", "pydantic.BaseModel"),
            OverrideRisk::SealedEquivalent
        );

        let final_pos = src.rfind("self.dict()").expect("second self.dict()");
        let final_self_recv = TextRange::new(final_pos as u32, (final_pos + 4) as u32);
        assert_eq!(
            oracle.method_override_risk(FileId(1), final_self_recv, "dict", "pydantic.BaseModel"),
            OverrideRisk::SealedEquivalent
        );
    }

    #[test]
    fn r24_intra_class_return_type_inference_resolves_self_attr_and_preserves_unknown_on_conflict()
    {
        let src = concat!(
            "from typing import Any\n\n",
            "class MyModel:\n",
            "    pass\n\n",
            "class BaseFactory:\n",
            "    model_cls = MyModel\n\n",
            "class SubFactory(BaseFactory):\n",
            "    def make(self):\n",
            "        return self.model_cls()\n\n",
            "    def make_any(self) -> Any:\n",
            "        return self.model_cls()\n\n",
            "    def ambiguous(self, flag: bool):\n",
            "        if flag:\n",
            "            return self.model_cls()\n",
            "        return 'other'\n\n",
            "def use(f: SubFactory):\n",
            "    a = f.make()\n",
            "    b = f.make_any()\n",
            "    c = f.ambiguous(True)\n",
            "    return a, b, c\n",
        );
        let oracle = TyOracleAdapter::new();
        oracle.index_single_file(FileId(1), "models.py", src);

        let target = CanonicalType::Nominal {
            fqname: "models.MyModel".to_string(),
            args: vec![],
        };

        let a_ty = oracle.type_of(FileId(1), span_of(src, "f.make()"));
        assert_eq!(oracle.is_subtype(&a_ty, &target), TriState::Yes);

        let b_ty = oracle.type_of(FileId(1), span_of(src, "f.make_any()"));
        assert_eq!(oracle.is_subtype(&b_ty, &target), TriState::Yes);

        let c_ty = oracle.type_of(FileId(1), span_of(src, "f.ambiguous(True)"));
        assert_eq!(oracle.is_subtype(&c_ty, &target), TriState::Unknown);
    }

    #[test]
    fn issue_6_variadic_tuple_salsa_and_ast_annotations_and_builtin_tuple_call_align() {
        let src = concat!(
            "from collections.abc import Iterable, Sequence\n",
            "from enum import Enum\n",
            "from typing import Tuple\n",
            "\n",
            "class Rule(str, Enum):\n",
            "    BP001 = 'BP001'\n",
            "\n",
            "def inspect(\n",
            "    lines: list[str],\n",
            "    explicit_modern: list[tuple[str, ...]],\n",
            "    explicit_typing: Tuple[str, ...],\n",
            "    fixed_one: list[tuple[int]],\n",
            "    rule_pair: tuple[Rule, Rule],\n",
            ") -> None:\n",
            "    suites = [tuple(line.split(':')) for line in lines]\n",
            "    direct_tup = tuple(lines)\n",
            "    _ = (suites, explicit_modern, explicit_typing, fixed_one, direct_tup, rule_pair)\n",
        );
        let oracle = TyOracleAdapter::new();
        oracle.index_single_file(FileId(1), "e2e.py", src);

        let str_ty = CanonicalType::Primitive(PrimitiveKind::Str);
        let int_ty = CanonicalType::Primitive(PrimitiveKind::Int);
        let expected_var_str = CanonicalType::VariadicTuple(Box::new(str_ty));
        let expected_list_var_str = CanonicalType::Nominal {
            fqname: "builtins.list".to_string(),
            args: vec![expected_var_str.clone()],
        };

        // 1. Salsa `ty` inferred `[tuple(line.split(':')) for line in lines]` -> `list[tuple[str, ...]]`
        let suites_ty = oracle.type_of(
            FileId(1),
            span_of(src, "[tuple(line.split(':')) for line in lines]"),
        );
        assert_eq!(suites_ty, expected_list_var_str);

        // 2. Explicit AST `list[tuple[str, ...]]` and `Tuple[str, ...]` match Salsa representation identically
        let explicit_mod_span = span_of(src, "(suites, explicit_modern");
        let explicit_mod_ty = oracle.type_of(
            FileId(1),
            TextRange::new(
                explicit_mod_span.start.0 + "(suites, ".len() as u32,
                explicit_mod_span.end.0,
            ),
        );
        assert_eq!(explicit_mod_ty, expected_list_var_str);

        let explicit_typ_span = span_of(src, "explicit_typing, fixed_one");
        let explicit_typ_ty = oracle.type_of(
            FileId(1),
            TextRange::new(
                explicit_typ_span.start.0,
                explicit_typ_span.start.0 + "explicit_typing".len() as u32,
            ),
        );
        assert_eq!(explicit_typ_ty, expected_var_str);

        // 3. Built-in `tuple(lines)` call propagates element type into `VariadicTuple(str)`
        let direct_tup_ty = oracle.type_of(FileId(1), span_of(src, "tuple(lines)"));
        assert_eq!(direct_tup_ty, expected_var_str);

        // 4. Fixed 1-tuple `list[tuple[int]]` stays distinct from variadic `list[tuple[int, ...]]`
        let fixed_one_span = span_of(src, "fixed_one, direct_tup");
        let fixed_one_ty = oracle.type_of(
            FileId(1),
            TextRange::new(
                fixed_one_span.start.0,
                fixed_one_span.start.0 + "fixed_one".len() as u32,
            ),
        );
        assert_eq!(
            fixed_one_ty,
            CanonicalType::Nominal {
                fqname: "builtins.list".to_string(),
                args: vec![CanonicalType::Nominal {
                    fqname: "builtins.tuple".to_string(),
                    args: vec![int_ty],
                }],
            }
        );

        // 5. MRO + covariant tuple / Sequence / Iterable subtyping (`tuple[Rule, Rule]` <: `tuple[str, ...]`, `Sequence[str]`, `Iterable[str]`)
        let rp_span = span_of(src, "rule_pair)");
        let rp_ty = oracle.type_of(
            FileId(1),
            TextRange::new(rp_span.start.0, rp_span.end.0 - 1),
        );
        assert_eq!(oracle.is_subtype(&rp_ty, &expected_var_str), TriState::Yes);
        assert_eq!(
            oracle.is_subtype(&rp_ty, &CanonicalType::parse_type_expr("Sequence[str]")),
            TriState::Yes
        );
        assert_eq!(
            oracle.is_subtype(&rp_ty, &CanonicalType::parse_type_expr("Iterable[str]")),
            TriState::Yes
        );
    }
}
