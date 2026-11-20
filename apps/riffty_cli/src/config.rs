use riffty_adapter_plugin::ArchitectureForbidRule;
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ArchitectureTomlConfig {
    pub forbid: Vec<ArchitectureForbidRule>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct JavaTomlConfig {
    pub architecture: ArchitectureTomlConfig,
}

/// Optional TOML configuration loaded from `riffty.toml` or `[tool.riffty]` in `pyproject.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct RifftyTomlConfig {
    pub rules: Option<PathBuf>,
    pub python_venv: Option<PathBuf>,
    pub exclude: Vec<String>,
    pub java: JavaTomlConfig,
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
}
