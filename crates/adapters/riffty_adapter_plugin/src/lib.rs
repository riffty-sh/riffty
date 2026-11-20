use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use riffty_core::{
    AnalysisProvenance, AnalysisTier, AnalyzePassRequest, AnalyzePassResponse, Applicability,
    Diagnostic, Edit, FileId, Fix, Language, LanguageAnalyzerPort, Severity, TextRange,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const MAX_STDOUT_BYTES: usize = 64 * 1024 * 1024; // 64 MiB safety cap

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchitectureForbidRule {
    pub id: String,
    pub from: String,
    pub target: Vec<String>,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct JavaSidecarBundle {
    pub java_bin: PathBuf,
    pub runner_jar: PathBuf,
    pub expected_sha256: String,
}

#[derive(Debug, Clone)]
pub struct JavaPluginAdapter {
    pub bundle: JavaSidecarBundle,
    pub extra_classpath: Vec<String>,
    pub use_daemon: bool,
    pub daemon_socket: Option<PathBuf>,
    pub architecture_forbid: Vec<ArchitectureForbidRule>,
}

/// Discovers a JDK 25+ binary and `riffty-java-runner.jar` + `riffty-java.sha256` manifest.
pub fn discover_java_sidecar_bundle() -> Option<JavaSidecarBundle> {
    let java_bin = discover_jdk25_bin()?;
    let runner_jar = discover_runner_jar()?;
    let sha_path = runner_jar.with_file_name("riffty-java.sha256");
    let expected_sha256 = fs::read_to_string(&sha_path)
        .ok()?
        .split_whitespace()
        .next()?
        .to_ascii_lowercase();
    Some(JavaSidecarBundle {
        java_bin,
        runner_jar,
        expected_sha256,
    })
}

pub fn discover_jdk25_bin() -> Option<PathBuf> {
    if let Ok(env_bin) = std::env::var("RIFFTY_JAVA_BIN") {
        let p = PathBuf::from(env_bin);
        if p.is_file() {
            return Some(p);
        }
    }
    let mut candidates = vec![
        PathBuf::from("/opt/homebrew/opt/openjdk@25/bin/java"),
        PathBuf::from("/opt/homebrew/opt/openjdk/bin/java"),
    ];
    if let Ok(home) = std::env::var("HOME") {
        candidates.push(PathBuf::from(&home).join(".sdkman/candidates/java/current/bin/java"));
        candidates.push(PathBuf::from(&home).join(".sdkman/candidates/java/25-tem/bin/java"));
        candidates.push(PathBuf::from(&home).join(".sdkman/candidates/java/25-open/bin/java"));
    }
    for cand in candidates {
        if cand.is_file() && is_jdk25_or_newer(&cand) {
            return Some(cand);
        }
    }
    let path_java = PathBuf::from("java");
    if is_jdk25_or_newer(&path_java) {
        return Some(path_java);
    }
    None
}

fn is_jdk25_or_newer(java_bin: &Path) -> bool {
    let Ok(out) = Command::new(java_bin).arg("-version").output() else {
        return false;
    };
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    text.contains("version \"25") || text.contains("version \"26") || text.contains("version \"27")
}

fn discover_runner_jar() -> Option<PathBuf> {
    if let Ok(env_jar) = std::env::var("RIFFTY_JAVA_JAR") {
        let p = PathBuf::from(env_jar);
        if p.is_file() {
            return Some(p);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        for anc in exe.ancestors() {
            let cand = anc.join("plugins/riffty-java/dist/riffty-java-runner.jar");
            if cand.is_file() {
                return Some(cand);
            }
            let sibling = anc.join("riffty-java-runner.jar");
            if sibling.is_file() {
                return Some(sibling);
            }
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        for anc in cwd.ancestors() {
            let cand = anc.join("riffty/plugins/riffty-java/dist/riffty-java-runner.jar");
            if cand.is_file() {
                return Some(cand);
            }
            let direct = anc.join("plugins/riffty-java/dist/riffty-java-runner.jar");
            if direct.is_file() {
                return Some(direct);
            }
        }
    }
    None
}

/// Computes SHA-256 hex digest of `jar_path` and verifies it matches `expected_hex`.
pub fn verify_jar_sha256(jar_path: &Path, expected_hex: &str) -> Result<(), String> {
    let bytes = fs::read(jar_path)
        .map_err(|e| format!("failed to read plugin jar `{}`: {e}", jar_path.display()))?;
    let digest = Sha256::digest(&bytes);
    let actual_hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    if actual_hex != expected_hex.trim().to_ascii_lowercase() {
        return Err(format!(
            "security boundary violation: SHA-256 mismatch for `{}` (expected {}, got {})",
            jar_path.display(),
            expected_hex.trim(),
            actual_hex
        ));
    }
    Ok(())
}

/// Resolves Level-0 static classpath entries without executing `./mvnw` or `./gradlew` (zero RCE).
pub fn resolve_level0_classpath(workspace_root: &Path, extra: &[String]) -> Vec<String> {
    let mut entries: Vec<String> = Vec::new();
    for rel in [
        "target/classes",
        "target/test-classes",
        "build/classes/java/main",
        "build/classes/java/test",
    ] {
        let cand = workspace_root.join(rel);
        if cand.is_dir() {
            entries.push(cand.to_string_lossy().to_string());
        }
    }
    for e in extra {
        if !e.trim().is_empty() && !entries.contains(e) {
            entries.push(e.clone());
        }
    }
    entries
}

fn build_hardened_java_command(bundle: &JavaSidecarBundle, daemon_mode: bool) -> Command {
    let mut cmd = Command::new(&bundle.java_bin);
    // Explicitly strip JAVA_HOME, JAVA_TOOL_OPTIONS, _JAVA_OPTIONS, JDK_JAVA_OPTIONS
    cmd.env_clear();
    for key in ["PATH", "TMPDIR", "SystemRoot"] {
        if let Some(val) = std::env::var_os(key) {
            cmd.env(key, val);
        }
    }
    let max_heap = std::env::var("RIFFTY_JAVA_MAX_HEAP").unwrap_or_else(|_| "1g".to_string());
    cmd.arg("-XX:+UseCompactObjectHeaders");
    if daemon_mode {
        cmd.arg("-XX:+UseZGC");
    } else {
        cmd.arg("-XX:+UseSerialGC");
    }
    cmd.arg(format!("-Xmx{max_heap}"));
    cmd.arg("-XX:+ExitOnOutOfMemoryError");
    cmd.args([
        "--add-exports=jdk.compiler/com.sun.tools.javac.api=ALL-UNNAMED",
        "--add-exports=jdk.compiler/com.sun.tools.javac.file=ALL-UNNAMED",
        "--add-exports=jdk.compiler/com.sun.tools.javac.main=ALL-UNNAMED",
        "--add-exports=jdk.compiler/com.sun.tools.javac.model=ALL-UNNAMED",
        "--add-exports=jdk.compiler/com.sun.tools.javac.parser=ALL-UNNAMED",
        "--add-exports=jdk.compiler/com.sun.tools.javac.processing=ALL-UNNAMED",
        "--add-exports=jdk.compiler/com.sun.tools.javac.tree=ALL-UNNAMED",
        "--add-exports=jdk.compiler/com.sun.tools.javac.util=ALL-UNNAMED",
        "--add-opens=jdk.compiler/com.sun.tools.javac.code=ALL-UNNAMED",
        "--add-opens=jdk.compiler/com.sun.tools.javac.comp=ALL-UNNAMED",
        "--add-opens=jdk.compiler/com.sun.tools.javac.tree=ALL-UNNAMED",
        "--add-opens=jdk.compiler/com.sun.tools.javac.util=ALL-UNNAMED",
    ]);
    cmd.arg("-jar").arg(&bundle.runner_jar);
    cmd
}

#[derive(Debug, Deserialize)]
struct WireResponse {
    #[allow(dead_code)]
    protocol: u32,
    #[allow(dead_code)]
    id: String,
    error: Option<String>,
    provenance: Option<AnalysisProvenance>,
    #[serde(default)]
    diagnostics: Vec<WireDiagnostic>,
}

#[derive(Debug, Deserialize)]
struct WireRange {
    start: u32,
    end: u32,
}

#[derive(Debug, Deserialize)]
struct WireEdit {
    range: WireRange,
    replacement: String,
}

#[derive(Debug, Deserialize)]
struct WireFix {
    title: String,
    edits: Vec<WireEdit>,
    applicability: Applicability,
}

#[derive(Debug, Deserialize)]
struct WireDiagnostic {
    rule_id: String,
    rule_name: String,
    severity: Severity,
    file_id: u32,
    range: WireRange,
    message: String,
    type_proof: Option<serde_json::Value>,
    fix: Option<WireFix>,
}

impl LanguageAnalyzerPort for JavaPluginAdapter {
    fn language(&self) -> Language {
        Language::Java
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["java"]
    }

    fn max_fixpoint_passes(&self) -> usize {
        2
    }

    fn analyze_pass(&self, req: &AnalyzePassRequest<'_>) -> Result<AnalyzePassResponse, String> {
        // Pre-exec SHA-256 supply-chain verification before every invocation
        verify_jar_sha256(&self.bundle.runner_jar, &self.bundle.expected_sha256)?;

        let classpath = resolve_level0_classpath(req.workspace_root, &self.extra_classpath);
        let wire_files: Vec<serde_json::Value> = req
            .files
            .iter()
            .filter(|wf| {
                req.scoped_ranges
                    .is_none_or(|scopes| scopes.contains_key(&wf.file_id))
            })
            .map(|wf| {
                serde_json::json!({
                    "file_id": wf.file_id.0,
                    "relative_path": wf.relative_path,
                    "source": wf.source,
                })
            })
            .collect();

        let payload = serde_json::json!({
            "protocol": 1,
            "id": format!("pass-{}", req.pass_index),
            "workspace_root": req.workspace_root.to_string_lossy(),
            "jdk_release": 25,
            "classpath": classpath,
            "files": wire_files,
            "allow_unsafe": req.allow_unsafe,
            "architecture_forbid": self.architecture_forbid,
        });
        let request_line = serde_json::to_string(&payload)
            .map_err(|e| format!("failed to serialize riffty-java request: {e}"))?;

        let response_line = if self.use_daemon {
            self.send_via_daemon(&request_line)?
        } else {
            self.send_via_stdio(&request_line)?
        };

        if response_line.len() > MAX_STDOUT_BYTES {
            return Err("riffty-java response exceeded 64 MiB safety cap".to_string());
        }

        let wire: WireResponse = serde_json::from_str(&response_line)
            .map_err(|e| format!("invalid JSON-RPC response from riffty-java: {e}"))?;

        if let Some(err) = wire.error {
            return Err(format!("riffty-java error: {err}"));
        }

        let provenance = wire.provenance.unwrap_or(AnalysisProvenance {
            language: Language::Java,
            tier: AnalysisTier::Tier2Semantic,
            files_analyzed: wire_files.len(),
            runtime_detail: Some("jdk 25 (javac trees)".to_string()),
            degraded_reason: None,
            remediation_hint: None,
        });

        let diagnostics: Vec<Diagnostic> = wire
            .diagnostics
            .into_iter()
            .map(|wd| Diagnostic {
                rule_id: wd.rule_id,
                rule_name: wd.rule_name,
                severity: wd.severity,
                file_id: FileId(wd.file_id),
                range: TextRange::new(wd.range.start, wd.range.end),
                message: wd.message,
                type_proof: wd.type_proof,
                fix: wd.fix.map(|wf| Fix {
                    title: wf.title,
                    edits: wf
                        .edits
                        .into_iter()
                        .map(|e| Edit {
                            range: TextRange::new(e.range.start, e.range.end),
                            replacement: e.replacement,
                        })
                        .collect(),
                    required_imports: Vec::new(),
                    applicability: wf.applicability,
                }),
            })
            .collect();

        Ok(AnalyzePassResponse {
            diagnostics,
            provenance,
        })
    }
}

impl JavaPluginAdapter {
    fn send_via_stdio(&self, request_line: &str) -> Result<String, String> {
        let mut cmd = build_hardened_java_command(&self.bundle, false);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("failed to spawn riffty-java sidecar: {e}"))?;

        let stderr_pipe = child.stderr.take();
        let stderr_handle = std::thread::spawn(move || {
            let mut buf = String::new();
            if let Some(mut s) = stderr_pipe {
                let _ = std::io::Read::read_to_string(&mut s, &mut buf);
            }
            buf
        });

        {
            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| "missing stdin pipe on riffty-java".to_string())?;
            writeln!(stdin, "{request_line}")
                .map_err(|e| format!("failed writing to riffty-java stdin: {e}"))?;
            stdin
                .flush()
                .map_err(|e| format!("failed flushing riffty-java stdin: {e}"))?;
        }

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "missing stdout pipe on riffty-java".to_string())?;
        let mut reader = BufReader::new(stdout);
        let mut response_line = String::new();
        reader
            .read_line(&mut response_line)
            .map_err(|e| format!("failed reading riffty-java stdout: {e}"))?;

        let status = child
            .wait()
            .map_err(|e| format!("failed waiting on riffty-java: {e}"))?;
        let stderr_text = stderr_handle.join().unwrap_or_default();

        if !status.success() || response_line.trim().is_empty() {
            return Err(format!(
                "riffty-java exited with {status}; stderr: {}",
                stderr_text.trim()
            ));
        }

        Ok(response_line)
    }

    #[cfg(unix)]
    fn send_via_daemon(&self, request_line: &str) -> Result<String, String> {
        use std::os::unix::net::UnixStream;

        let sock_path = self.daemon_socket.clone().unwrap_or_else(|| {
            std::env::temp_dir().join(format!("riffty-java-daemon-{}.sock", std::process::id()))
        });

        let stream = match UnixStream::connect(&sock_path) {
            Ok(s) => s,
            Err(_) => {
                // Start warm daemon in background and wait up to 5s for Unix socket readiness
                let mut cmd = build_hardened_java_command(&self.bundle, true);
                cmd.arg("--daemon")
                    .arg(&sock_path)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                let _child = cmd
                    .spawn()
                    .map_err(|e| format!("failed to spawn riffty-java warm daemon: {e}"))?;

                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    if let Ok(s) = UnixStream::connect(&sock_path) {
                        break s;
                    }
                    if Instant::now() >= deadline {
                        return Err(format!(
                            "timed out waiting for riffty-java warm daemon at `{}`",
                            sock_path.display()
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
            }
        };

        let _ = stream.set_read_timeout(Some(Duration::from_secs(60)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));

        let mut writer = stream
            .try_clone()
            .map_err(|e| format!("failed cloning unix stream: {e}"))?;
        writeln!(writer, "{request_line}")
            .map_err(|e| format!("failed writing to riffty-java daemon: {e}"))?;
        writer
            .flush()
            .map_err(|e| format!("failed flushing riffty-java daemon: {e}"))?;

        let mut reader = BufReader::new(stream);
        let mut response_line = String::new();
        reader
            .read_line(&mut response_line)
            .map_err(|e| format!("failed reading from riffty-java daemon: {e}"))?;
        if response_line.trim().is_empty() {
            return Err("riffty-java warm daemon returned empty response".to_string());
        }
        Ok(response_line)
    }

    #[cfg(not(unix))]
    fn send_via_daemon(&self, request_line: &str) -> Result<String, String> {
        self.send_via_stdio(request_line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use riffty_core::WorkspaceFile;

    #[test]
    fn sha256_verification_rejects_tampered_jar() {
        let tmp = std::env::temp_dir().join("riffty_sha256_tamper_test.jar");
        fs::write(&tmp, b"tampered jar bytes").unwrap();
        let err = verify_jar_sha256(&tmp, "0000000000000000000000000000000000000000000000000000000000000000")
            .unwrap_err();
        assert!(err.contains("SHA-256 mismatch"), "err: {err}");
        let _ = fs::remove_file(&tmp);
    }

    #[test]
    fn tier2_sidecar_resolves_double_variable_bigdecimal_and_hoists_object_mapper() {
        let Some(bundle) = discover_java_sidecar_bundle() else {
            // Skip gracefully on machines without JDK 25 / built jar
            return;
        };
        let adapter = JavaPluginAdapter {
            bundle,
            extra_classpath: Vec::new(),
            use_daemon: false,
            daemon_socket: None,
            architecture_forbid: vec![ArchitectureForbidRule {
                id: "ARCH-DOMAIN-001".to_string(),
                from: "com.acme.domain.**".to_string(),
                target: vec!["org.springframework.**".to_string()],
                message: "Domain layer must stay framework-free".to_string(),
            }],
        };

        let src = r#"package com.acme.domain.billing;
import java.math.BigDecimal;
import org.springframework.stereotype.Service;

class BillingService {
    void charge(double amount) throws Exception {
        BigDecimal bd = new BigDecimal(amount);
        ObjectMapper m = new ObjectMapper();
        m.writeValueAsString(bd);
    }
}
"#;
        let files = vec![WorkspaceFile {
            file_id: FileId(1),
            path: PathBuf::from("BillingService.java"),
            relative_path: "BillingService.java".to_string(),
            source: src.to_string(),
        }];

        let resp = adapter
            .analyze_pass(&AnalyzePassRequest {
                workspace_root: Path::new("."),
                files: &files,
                scoped_ranges: None,
                rules_paths: &[],
                allow_unsafe: false,
                pass_index: 0,
            })
            .expect("Tier 2 javac sidecar should succeed");

        assert_eq!(resp.provenance.tier, AnalysisTier::Tier2Semantic);
        let ids: Vec<&str> = resp.diagnostics.iter().map(|d| d.rule_id.as_str()).collect();
        assert!(ids.contains(&"ARCH-DOMAIN-001"), "ids: {ids:?}");
        assert!(ids.contains(&"JAVA-BIGDEC-001"), "ids: {ids:?}");
        assert!(ids.contains(&"JAVA-OBJMAP-001"), "ids: {ids:?}");

        let objmap = resp
            .diagnostics
            .iter()
            .find(|d| d.rule_id == "JAVA-OBJMAP-001")
            .unwrap();
        assert_eq!(objmap.fix.as_ref().unwrap().applicability, Applicability::Safe);
        assert_eq!(objmap.fix.as_ref().unwrap().edits.len(), 2);
    }
}

