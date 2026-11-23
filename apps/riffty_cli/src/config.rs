use riffty_adapter_plugin::ArchitectureForbidRule;
use riffty_adapter_tsrs::JsTsArchitectureForbidRule;
use riffty_core::architecture::{ArchForbidRule, ArchLayer, ArchitectureConfig};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ArchitectureTomlConfig {
    pub forbid: Vec<ArchitectureForbidRule>,
    pub layers: Vec<ArchLayer>,
    pub forbid_cycles: bool,
    pub cycle_slice_depth: Option<usize>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct JavaTomlConfig {
    pub architecture: ArchitectureTomlConfig,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct JsTsArchitectureTomlConfig {
    pub forbid: Vec<JsTsArchitectureForbidRule>,
    pub layers: Vec<ArchLayer>,
    pub forbid_cycles: bool,
    pub cycle_slice_depth: Option<usize>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct JsTsTomlConfig {
    pub architecture: JsTsArchitectureTomlConfig,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct PythonTomlConfig {
    pub architecture: ArchitectureConfig,
}

/// Optional TOML configuration loaded from `riffty.toml` or `[tool.riffty]` in `pyproject.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct RifftyTomlConfig {
    pub rules: Option<PathBuf>,
    pub python_venv: Option<PathBuf>,
    pub exclude: Vec<String>,
    pub architecture: ArchitectureConfig,
    pub java: JavaTomlConfig,
    pub jsts: JsTsTomlConfig,
    pub python: PythonTomlConfig,
}

impl RifftyTomlConfig {
    /// Combines top-level `[architecture]` with `[python.architecture]`, `[java.architecture]`
    /// (`layers`/`forbid_cycles`), and `[jsts.architecture]` (`layers`/`forbid_cycles`) into a
    /// unified cross-language `ArchitectureConfig`.
    ///
    /// Note: `java.architecture.forbid` and `jsts.architecture.forbid` continue to run in their
    /// native semantic analyzers as well; top-level `[architecture]` and `[python.architecture]`
    /// run through the shared Rust evaluator.
    pub fn merged_architecture_config(&self) -> ArchitectureConfig {
        let mut merged = self.architecture.clone();
        merged
            .forbid
            .extend(self.python.architecture.forbid.clone());
        merged
            .layers
            .extend(self.python.architecture.layers.clone());
        merged.layers.extend(self.java.architecture.layers.clone());
        merged.layers.extend(self.jsts.architecture.layers.clone());
        merged.forbid_cycles = merged.forbid_cycles
            || self.python.architecture.forbid_cycles
            || self.java.architecture.forbid_cycles
            || self.jsts.architecture.forbid_cycles;
        if merged.cycle_slice_depth.is_none() {
            merged.cycle_slice_depth = self
                .python
                .architecture
                .cycle_slice_depth
                .or(self.java.architecture.cycle_slice_depth)
                .or(self.jsts.architecture.cycle_slice_depth);
        }
        // Also mirror java forbid rules when running in Java Tier 1 fallback mode
        // (deduplicated against Tier 2 diagnostics by file_id + range + rule_id).
        for jf in &self.java.architecture.forbid {
            merged.forbid.push(ArchForbidRule {
                id: jf.id.clone(),
                from: jf.from.clone(),
                target: jf.target.clone(),
                message: Some(jf.message.clone()),
            });
        }
        merged
    }
}

#[derive(Debug, Default, Deserialize)]
struct PyProjectToml {
    tool: Option<PyProjectTool>,
}

#[derive(Debug, Default, Deserialize)]
struct PyProjectTool {
    riffty: Option<RifftyTomlConfig>,
}

impl RifftyTomlConfig {
    /// Discover and parse `riffty.toml` first, or `[tool.riffty]` in `pyproject.toml`
    /// under `workspace_root`. Returns `Default::default()` if neither file exists.
    pub fn discover(workspace_root: &Path) -> Result<Self, String> {
        let riffty_toml = workspace_root.join("riffty.toml");
        if riffty_toml.is_file() {
            let content = std::fs::read_to_string(&riffty_toml)
                .map_err(|e| format!("failed to read {}: {e}", riffty_toml.display()))?;
            return toml::from_str::<RifftyTomlConfig>(&content)
                .map_err(|e| format!("failed to parse {}: {e}", riffty_toml.display()));
        }

        let pyproject_toml = workspace_root.join("pyproject.toml");
        if pyproject_toml.is_file() {
            let content = std::fs::read_to_string(&pyproject_toml)
                .map_err(|e| format!("failed to read {}: {e}", pyproject_toml.display()))?;
            let parsed = toml::from_str::<PyProjectToml>(&content)
                .map_err(|e| format!("failed to parse {}: {e}", pyproject_toml.display()))?;
            return Ok(parsed.tool.and_then(|t| t.riffty).unwrap_or_default());
        }

        Ok(Self::default())
    }
}

/// Returns `true` if `relative_path` matches any entry in `exclude` (exact file or directory prefix).
pub fn is_excluded(relative_path: &str, exclude: &[String]) -> bool {
    let rel = relative_path
        .replace('\\', "/")
        .trim_start_matches("./")
        .trim_start_matches('/')
        .to_string();

    exclude.iter().any(|raw_pat| {
        let normalized_pat = raw_pat.replace('\\', "/");
        let pat = normalized_pat
            .trim_start_matches("./")
            .trim_start_matches('/')
            .trim_end_matches('/');
        !pat.is_empty() && (rel == pat || rel.starts_with(&format!("{pat}/")))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn make_temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("riffty_cfg_test_{name}_{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn loads_riffty_toml() {
        let dir = make_temp_dir("riffty_toml");
        std::fs::write(
            dir.join("riffty.toml"),
            r#"
rules = "custom_rules.yaml"
python_venv = ".venv"
exclude = ["tests/fixtures", "vendor/legacy.py"]
"#,
        )
        .unwrap();

        let cfg = RifftyTomlConfig::discover(&dir).unwrap();
        assert_eq!(cfg.rules, Some(PathBuf::from("custom_rules.yaml")));
        assert_eq!(cfg.python_venv, Some(PathBuf::from(".venv")));
        assert_eq!(
            cfg.exclude,
            vec!["tests/fixtures".to_string(), "vendor/legacy.py".to_string()]
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn loads_pyproject_tool_riffty() {
        let dir = make_temp_dir("pyproject");
        std::fs::write(
            dir.join("pyproject.toml"),
            r#"
[project]
name = "demo"

[tool.riffty]
rules = "rules/refaster.yaml"
python_venv = "/opt/venv"
exclude = ["build/"]
"#,
        )
        .unwrap();

        let cfg = RifftyTomlConfig::discover(&dir).unwrap();
        assert_eq!(cfg.rules, Some(PathBuf::from("rules/refaster.yaml")));
        assert_eq!(cfg.python_venv, Some(PathBuf::from("/opt/venv")));
        assert_eq!(cfg.exclude, vec!["build/".to_string()]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn riffty_toml_takes_precedence_over_pyproject_toml() {
        let dir = make_temp_dir("precedence");
        std::fs::write(dir.join("riffty.toml"), "rules = \"from_riffty.yaml\"\n").unwrap();
        std::fs::write(
            dir.join("pyproject.toml"),
            "[tool.riffty]\nrules = \"from_pyproject.yaml\"\n",
        )
        .unwrap();

        let cfg = RifftyTomlConfig::discover(&dir).unwrap();
        assert_eq!(cfg.rules, Some(PathBuf::from("from_riffty.yaml")));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn exclude_matches_exact_and_directory_prefix() {
        let excludes = vec![
            "tests/fixtures/".to_string(),
            "vendor/legacy.py".to_string(),
        ];
        assert!(is_excluded("tests/fixtures/bad.py", &excludes));
        assert!(is_excluded("./vendor/legacy.py", &excludes));
        assert!(!is_excluded("tests/fixtures_other/ok.py", &excludes));
        assert!(!is_excluded("src/app.py", &excludes));
    }

    #[test]
    fn cli_flags_take_precedence_over_toml_config() {
        let dir = make_temp_dir("cli_precedence");
        std::fs::write(
            dir.join("riffty.toml"),
            "rules = \"toml_rules.yaml\"\npython_venv = \"toml_venv\"\n",
        )
        .unwrap();

        let toml_cfg = RifftyTomlConfig::discover(&dir).unwrap();
        let cli_rules = Some(PathBuf::from("cli_rules.yaml"));
        let cli_venv: Option<PathBuf> = None;

        assert_eq!(
            cli_rules.or(toml_cfg.rules),
            Some(PathBuf::from("cli_rules.yaml"))
        );
        assert_eq!(
            cli_venv.or(toml_cfg.python_venv),
            Some(PathBuf::from("toml_venv"))
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn loads_cross_language_architecture_layers_and_cycles() {
        let dir = make_temp_dir("arch_cfg");
        std::fs::write(
            dir.join("riffty.toml"),
            r#"
[architecture]
forbid_cycles = true

[[architecture.layers]]
name = "domain"
matches = ["com.acme.domain.**", "app.domain.**"]
may_access = []

[[architecture.layers]]
name = "infra"
matches = ["com.acme.infra.**", "app.infra.**"]
may_access = ["domain"]
"#,
        )
        .unwrap();

        let cfg = RifftyTomlConfig::discover(&dir).unwrap();
        let merged = cfg.merged_architecture_config();
        assert!(merged.forbid_cycles);
        assert_eq!(merged.layers.len(), 2);
        assert_eq!(merged.layers[0].name, "domain");
        assert_eq!(merged.layers[1].may_access, vec!["domain".to_string()]);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
