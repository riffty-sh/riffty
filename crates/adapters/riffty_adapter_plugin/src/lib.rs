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
    pub aot_cache: Option<(PathBuf, String)>,
}

#[derive(Debug, Clone)]
pub struct JavaPluginAdapter {
    pub bundle: JavaSidecarBundle,
    pub extra_classpath: Vec<String>,
    pub use_daemon: bool,
    pub daemon_socket: Option<PathBuf>,
    pub architecture_forbid: Vec<ArchitectureForbidRule>,
    /// Level-1 build-tool fallback (`mvn -o dependency:build-classpath`). MUST stay `false` in `riffty mcp`.
    pub allow_build_tool: bool,
}

/// Discovers a bundled `jlink` JDK 25 runtime (or system JDK 25+), `riffty-java-runner.jar`,
/// optional Project Leyden `riffty-java.aot`, and their pinned SHA-256 manifests.
pub fn discover_java_sidecar_bundle() -> Option<JavaSidecarBundle> {
    let runner_jar = discover_runner_jar()?;
    let sha_path = runner_jar.with_file_name("riffty-java.sha256");
    let expected_sha256 = fs::read_to_string(&sha_path)
        .ok()?
        .split_whitespace()
        .next()?
        .to_ascii_lowercase();

    // Prefer bundled jlink JDK 25 runtime right beside runner_jar (zero JAVA_HOME needed)
    let bundled_java = runner_jar
        .parent()
        .map(|d| d.join("runtime/bin/java"))
        .filter(|p| p.is_file());
    let java_bin = bundled_java.or_else(discover_jdk25_bin)?;

    let aot_cache = runner_jar.parent().and_then(|d| {
        let aot_path = d.join("riffty-java.aot");
        let aot_sha_path = d.join("riffty-java.aot.sha256");
        if aot_path.is_file()
            && let Ok(raw) = fs::read_to_string(&aot_sha_path)
            && let Some(hex) = raw.split_whitespace().next()
        {
            return Some((aot_path, hex.to_ascii_lowercase()));
        }
        None
    });

    Some(JavaSidecarBundle {
        java_bin,
        runner_jar,
        expected_sha256,
        aot_cache,
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

/// Resolves Level-0 static classpath entries without executing `./mvnw` or `./gradlew` (zero RCE):
/// 1. Existing compiled output directories (`target/classes`, `build/classes/java/main`, etc.)
/// 2. Static `pom.xml` `<dependency>` coordinates mapped to read-only `~/.m2/repository`
/// 3. Static `gradle.lockfile` coordinates mapped to read-only `~/.gradle/caches/modules-2/files-2.1`
/// 4. Explicit `--java-classpath` / `RIFFTY_JAVA_CLASSPATH` entries.
pub fn resolve_level0_classpath(workspace_root: &Path, extra: &[String]) -> Vec<String> {
    let mut entries: Vec<String> = Vec::new();
    let mut push_unique = |val: String| {
        if !val.trim().is_empty() && !entries.contains(&val) {
            entries.push(val);
        }
    };

    for rel in [
        "target/classes",
        "target/test-classes",
        "build/classes/java/main",
        "build/classes/java/test",
    ] {
        let cand = workspace_root.join(rel);
        if cand.is_dir() {
            push_unique(cand.to_string_lossy().to_string());
        }
    }

    // Static pom.xml -> ~/.m2/repository
    let pom_path = workspace_root.join("pom.xml");
    if pom_path.is_file()
        && let Ok(pom_xml) = fs::read_to_string(&pom_path)
    {
        let m2_root = std::env::var("RIFFTY_M2_REPO")
            .map(PathBuf::from)
            .ok()
            .or_else(|| {
                std::env::var("HOME")
                    .ok()
                    .map(|h| PathBuf::from(h).join(".m2/repository"))
            });
        if let Some(m2) = m2_root {
            for (g, a, v) in parse_pom_dependencies(&pom_xml) {
                let jar = m2
                    .join(g.replace('.', "/"))
                    .join(&a)
                    .join(&v)
                    .join(format!("{a}-{v}.jar"));
                if jar.is_file() {
                    push_unique(jar.to_string_lossy().to_string());
                }
            }
        }
    }

    // Static gradle.lockfile -> ~/.gradle/caches/modules-2/files-2.1
    let lock_path = workspace_root.join("gradle.lockfile");
    if lock_path.is_file()
        && let Ok(lock_txt) = fs::read_to_string(&lock_path)
    {
        let gradle_root = std::env::var("RIFFTY_GRADLE_CACHE")
            .map(PathBuf::from)
            .ok()
            .or_else(|| {
                std::env::var("HOME")
                    .ok()
                    .map(|h| PathBuf::from(h).join(".gradle/caches/modules-2/files-2.1"))
            });
        if let Some(gcache) = gradle_root {
            for (g, a, v) in parse_gradle_lockfile(&lock_txt) {
                let ver_dir = gcache.join(&g).join(&a).join(&v);
                if let Ok(hash_dirs) = fs::read_dir(&ver_dir) {
                    for entry in hash_dirs.flatten() {
                        let jar = entry.path().join(format!("{a}-{v}.jar"));
                        if jar.is_file() {
                            push_unique(jar.to_string_lossy().to_string());
                            break;
                        }
                    }
                }
            }
        }
    }

    for e in extra {
        push_unique(e.clone());
    }
    entries
}

/// Opt-in Level-1 build-tool classpath resolution (`--allow-build-tool`, hard-disabled in MCP).
/// Runs offline Maven `dependency:build-classpath` and caches under `.riffty/cache/java-cp-<sha256>.json`.
pub fn resolve_level1_build_tool_classpath(workspace_root: &Path) -> Vec<String> {
    let pom_path = workspace_root.join("pom.xml");
    let Ok(pom_bytes) = fs::read(&pom_path) else {
        return Vec::new();
    };
    let digest = Sha256::digest(&pom_bytes);
    let sha_hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    let cache_dir = workspace_root.join(".riffty/cache");
    let cache_file = cache_dir.join(format!("java-cp-{}.json", &sha_hex[..16]));
    if let Ok(cached) = fs::read_to_string(&cache_file)
        && let Ok(parsed) = serde_json::from_str::<Vec<String>>(&cached)
    {
        return parsed;
    }

    let _ = fs::create_dir_all(&cache_dir);
    let tmp_cp = cache_dir.join(format!("mvn-cp-{}.txt", std::process::id()));
    let status = Command::new("mvn")
        .current_dir(workspace_root)
        .args([
            "-q",
            "-o",
            "dependency:build-classpath",
            &format!("-Dmdep.outputFile={}", tmp_cp.display()),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    let mut resolved = Vec::new();
    if status.is_ok_and(|s| s.success())
        && let Ok(raw) = fs::read_to_string(&tmp_cp)
    {
        for item in raw.split([':', ';']) {
            let trimmed = item.trim();
            if !trimmed.is_empty() && Path::new(trimmed).exists() {
                resolved.push(trimmed.to_string());
            }
        }
        if let Ok(json) = serde_json::to_string(&resolved) {
            let _ = fs::write(&cache_file, json);
        }
    }
    let _ = fs::remove_file(&tmp_cp);
    resolved
}

fn parse_pom_dependencies(xml: &str) -> Vec<(String, String, String)> {
    use std::collections::HashMap;
    let mut props: HashMap<String, String> = HashMap::new();
    if let Some(p_start) = xml.find("<properties>")
        && let Some(p_end) = xml[p_start..].find("</properties>")
    {
        let block = &xml[p_start + "<properties>".len()..p_start + p_end];
        let mut cursor = block;
        while let Some(lt) = cursor.find('<') {
            cursor = &cursor[lt + 1..];
            let Some(gt) = cursor.find('>') else { break };
            let tag = cursor[..gt].trim();
            if tag.is_empty() || tag.starts_with('/') || tag.starts_with('!') || tag.starts_with('?') {
                cursor = &cursor[gt + 1..];
                continue;
            }
            let close_tag = format!("</{tag}>");
            let after = &cursor[gt + 1..];
            if let Some(c_idx) = after.find(&close_tag) {
                let val = after[..c_idx].trim();
                if !val.is_empty() && !val.contains('<') {
                    props.insert(tag.to_string(), val.to_string());
                }
                cursor = &after[c_idx + close_tag.len()..];
            } else {
                cursor = after;
            }
        }
    }

    let mut deps = Vec::new();
    let mut cursor = xml;
    while let Some(d_start) = cursor.find("<dependency>") {
        let after = &cursor[d_start + "<dependency>".len()..];
        let Some(d_end) = after.find("</dependency>") else {
            break;
        };
        let block = &after[..d_end];
        let g = extract_xml_tag(block, "groupId");
        let a = extract_xml_tag(block, "artifactId");
        let v = extract_xml_tag(block, "version").map(|raw| substitute_pom_props(&raw, &props));
        if let (Some(group), Some(artifact), Some(version)) = (g, a, v)
            && !version.contains("${")
        {
            deps.push((group, artifact, version));
        }
        cursor = &after[d_end + "</dependency>".len()..];
    }
    deps
}

fn extract_xml_tag(block: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let s = block.find(&open)? + open.len();
    let e = block[s..].find(&close)?;
    let val = block[s..s + e].trim();
    if val.is_empty() {
        None
    } else {
        Some(val.to_string())
    }
}

fn substitute_pom_props(raw: &str, props: &std::collections::HashMap<String, String>) -> String {
    let mut out = raw.to_string();
    for (k, v) in props {
        out = out.replace(&format!("${{{k}}}"), v);
    }
    out
}

fn parse_gradle_lockfile(content: &str) -> Vec<(String, String, String)> {
    let mut deps = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with("empty=") {
            continue;
        }
        let coords = trimmed.split('=').next().unwrap_or("");
        let mut parts = coords.split(':');
        if let (Some(g), Some(a), Some(v)) = (parts.next(), parts.next(), parts.next())
            && !g.is_empty()
            && !a.is_empty()
            && !v.is_empty()
        {
            deps.push((g.to_string(), a.to_string(), v.to_string()));
        }
    }
    deps
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
        if let Some((ref aot_path, _)) = bundle.aot_cache {
            cmd.arg(format!("-XX:AOTCache={}", aot_path.display()));
        }
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
        5
    }

    fn analyze_pass(&self, req: &AnalyzePassRequest<'_>) -> Result<AnalyzePassResponse, String> {
        // Pre-exec SHA-256 supply-chain verification before every invocation
        verify_jar_sha256(&self.bundle.runner_jar, &self.bundle.expected_sha256)?;
        if let Some((ref aot_path, ref expected_aot_sha)) = self.bundle.aot_cache {
            verify_jar_sha256(aot_path, expected_aot_sha)?;
        }

        let mut classpath = resolve_level0_classpath(req.workspace_root, &self.extra_classpath);
        if self.allow_build_tool {
            for entry in resolve_level1_build_tool_classpath(req.workspace_root) {
                if !classpath.contains(&entry) {
                    classpath.push(entry);
                }
            }
        }
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
            allow_build_tool: false,
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

    #[test]
    fn tier2_error_prone_bridge_catches_equals_hashcode_and_self_equals() {
        let Some(bundle) = discover_java_sidecar_bundle() else {
            return;
        };
        let adapter = JavaPluginAdapter {
            bundle,
            extra_classpath: Vec::new(),
            use_daemon: false,
            daemon_socket: None,
            architecture_forbid: Vec::new(),
            allow_build_tool: false,
        };
        let src = r#"package com.acme;
class BadKey {
    final String id;
    BadKey(String id) { this.id = id; }
    @Override
    public boolean equals(Object o) {
        return o instanceof BadKey && this.id.equals(this.id);
    }
}
"#;
        let files = vec![WorkspaceFile {
            file_id: FileId(1),
            path: PathBuf::from("BadKey.java"),
            relative_path: "BadKey.java".to_string(),
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
            .expect("Error Prone Tier 2 pass should succeed");
        let ids: Vec<&str> = resp.diagnostics.iter().map(|d| d.rule_id.as_str()).collect();
        assert!(
            ids.contains(&"EP-EqualsHashCode") || ids.contains(&"EP-SelfEquals"),
            "Expected Error Prone diagnostic (EP-EqualsHashCode / EP-SelfEquals), got: {ids:?} (detail={:?})",
            resp.provenance.runtime_detail
        );
    }

    #[test]
    fn static_pom_and_gradle_lockfile_resolve_jars_without_build_tool() {
        let tmp = std::env::temp_dir().join(format!("riffty_cp_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let pom = r#"
<project>
  <properties>
    <jackson.version>2.18.2</jackson.version>
  </properties>
  <dependencies>
    <dependency>
      <groupId>com.fasterxml.jackson.core</groupId>
      <artifactId>jackson-databind</artifactId>
      <version>${jackson.version}</version>
    </dependency>
  </dependencies>
</project>
"#;
        let deps = parse_pom_dependencies(pom);
        assert_eq!(
            deps,
            vec![(
                "com.fasterxml.jackson.core".to_string(),
                "jackson-databind".to_string(),
                "2.18.2".to_string()
            )]
        );

        let lock = "# Gradle lockfile\ncom.google.guava:guava:33.3.1-jre=compileClasspath\nempty=\n";
        let gdeps = parse_gradle_lockfile(lock);
        assert_eq!(
            gdeps,
            vec![(
                "com.google.guava".to_string(),
                "guava".to_string(),
                "33.3.1-jre".to_string()
            )]
        );
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn tier2_regex_hoists_inline_chained_constant_and_fails_closed_on_dynamic() {
        let Some(bundle) = discover_java_sidecar_bundle() else {
            return;
        };
        let adapter = JavaPluginAdapter {
            bundle,
            extra_classpath: Vec::new(),
            use_daemon: false,
            daemon_socket: None,
            architecture_forbid: Vec::new(),
            allow_build_tool: false,
        };

        let src = r#"package com.acme.security;
import java.util.regex.Pattern;

class TokenValidator {
    private static final Pattern ALREADY_HOISTED = Pattern.compile("^[0-9]+$");

    boolean isValidToken(String token) {
        return Pattern.compile("^[a-zA-Z0-9_-]{1,64}$").matcher(token).matches();
    }

    boolean isCaseInsensitiveValid(String token) {
        return Pattern.compile("^[a-z]+$", Pattern.CASE_INSENSITIVE).matcher(token).matches();
    }

    boolean matchesDynamic(String userRegex, String token) {
        return Pattern.compile(userRegex).matcher(token).matches();
    }
}
"#;
        let files = vec![WorkspaceFile {
            file_id: FileId(1),
            path: PathBuf::from("TokenValidator.java"),
            relative_path: "TokenValidator.java".to_string(),
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

        let regex_diags: Vec<&Diagnostic> = resp
            .diagnostics
            .iter()
            .filter(|d| d.rule_id == "JAVA-REGEX-001")
            .collect();
        assert_eq!(
            regex_diags.len(),
            3,
            "Expected 3 JAVA-REGEX-001 diagnostics (2 constant chained + 1 dynamic, excluding ALREADY_HOISTED), got: {regex_diags:?}"
        );

        let chained_1 = regex_diags
            .iter()
            .find(|d| {
                src[d.range.start.0 as usize..d.range.end.0 as usize]
                    .contains("^[a-zA-Z0-9_-]{1,64}$")
            })
            .expect("Expected JAVA-REGEX-001 for chained constant Pattern.compile");
        let fix_1 = chained_1.fix.as_ref().expect("fix expected");
        assert_eq!(fix_1.applicability, Applicability::Safe);
        assert_eq!(fix_1.edits.len(), 2);

        // Apply fix_1 edits bottom-to-top to verify clean hoisting
        let mut patched = src.to_string();
        let mut edits = fix_1.edits.clone();
        edits.sort_by_key(|e| std::cmp::Reverse(e.range.start));
        for e in edits {
            patched.replace_range(e.range.start.0 as usize..e.range.end.0 as usize, &e.replacement);
        }
        assert!(
            patched.contains("private static final Pattern RIFFTY_PATTERN_1 = Pattern.compile(\"^[a-zA-Z0-9_-]{1,64}$\");"),
            "patched:\n{patched}"
        );
        assert!(
            patched.contains("return RIFFTY_PATTERN_1.matcher(token).matches();"),
            "patched:\n{patched}"
        );

        let chained_2 = regex_diags
            .iter()
            .find(|d| {
                src[d.range.start.0 as usize..d.range.end.0 as usize]
                    .contains("Pattern.CASE_INSENSITIVE")
            })
            .expect("Expected JAVA-REGEX-001 for 2-arg constant Pattern.compile");
        let fix_2 = chained_2.fix.as_ref().expect("fix expected");
        assert_eq!(fix_2.applicability, Applicability::Safe);
        assert!(
            fix_2.edits.iter().any(|e| e.replacement == "RIFFTY_PATTERN_2"),
            "Expected distinct field name RIFFTY_PATTERN_2, got: {:?}",
            fix_2.edits
        );

        let dynamic = regex_diags
            .iter()
            .find(|d| {
                src[d.range.start.0 as usize..d.range.end.0 as usize].contains("userRegex")
            })
            .expect("Expected JAVA-REGEX-001 for dynamic Pattern.compile");
        let dynamic_fix = dynamic.fix.as_ref().expect("fix expected");
        assert_eq!(dynamic_fix.applicability, Applicability::ManualOnly);
        assert!(dynamic_fix.edits.is_empty());
    }
}

