use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const ALLOWED_HOST_ENV_KEYS: &[&str] = &[
    "PATH",
    "HOME",
    "TMPDIR",
    "USER",
    "LANG",
    "LC_ALL",
    "NO_COLOR",
    "JAVA_HOME",
    "VIRTUAL_ENV",
    "PYTHONDONTWRITEBYTECODE",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxMode {
    None,
    Docker { image: String },
}

#[derive(Debug, Clone)]
pub struct ExecResult {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub duration_ms: u64,
    pub stdout: String,
    pub stderr: String,
}

/// Executes `command` inside `cwd` with a sanitized environment allowlist,
/// hard timeout enforcement, and optional Docker container isolation.
pub fn run_sandboxed_command(
    cwd: &Path,
    command: &str,
    timeout_secs: u64,
    extra_env: &BTreeMap<String, String>,
    sandbox: &SandboxMode,
) -> Result<ExecResult, String> {
    let mut cmd = match sandbox {
        SandboxMode::None => {
            let mut c = Command::new("/bin/sh");
            c.arg("-c").arg(command);
            c.current_dir(cwd);
            c
        }
        SandboxMode::Docker { image } => {
            let abs_cwd = cwd
                .canonicalize()
                .map_err(|e| format!("failed to canonicalize {}: {e}", cwd.display()))?;
            let mut c = Command::new("docker");
            c.args([
                "run",
                "--rm",
                "--network=none",
                "-v",
                &format!("{}:/workspace", abs_cwd.display()),
                "-w",
                "/workspace",
            ]);
            for (k, v) in extra_env {
                c.arg("-e").arg(format!("{k}={v}"));
            }
            c.arg(image).args(["/bin/sh", "-c", command]);
            c
        }
    };

    cmd.env_clear();
    for key in ALLOWED_HOST_ENV_KEYS {
        if let Ok(val) = std::env::var(key) {
            cmd.env(key, val);
        }
    }
    if matches!(sandbox, SandboxMode::None) {
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
    }

    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let start = Instant::now();
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn command '{command}': {e}"))?;

    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();

    let stdout_handle = thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(ref mut p) = stdout_pipe {
            let _ = p.read_to_end(&mut buf);
        }
        String::from_utf8_lossy(&buf).into_owned()
    });
    let stderr_handle = thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(ref mut p) = stderr_pipe {
            let _ = p.read_to_end(&mut buf);
        }
        String::from_utf8_lossy(&buf).into_owned()
    });

    let deadline = Duration::from_secs(timeout_secs);
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) => {
                if start.elapsed() >= deadline {
                    timed_out = true;
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                thread::sleep(Duration::from_millis(25));
            }
            Err(e) => {
                let _ = child.kill();
                return Err(format!("error waiting for command '{command}': {e}"));
            }
        }
    };

    let duration_ms = start.elapsed().as_millis() as u64;
    let stdout = stdout_handle.join().unwrap_or_default();
    let stderr = stderr_handle.join().unwrap_or_default();

    Ok(ExecResult {
        exit_code: status.and_then(|s| s.code()),
        timed_out,
        duration_ms,
        stdout,
        stderr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enforces_timeout_and_sanitizes_host_environment() {
        let tmp = std::env::temp_dir();

        // Timeout test
        let timed = run_sandboxed_command(&tmp, "sleep 5", 1, &BTreeMap::new(), &SandboxMode::None)
            .expect("command should spawn");
        assert!(timed.timed_out, "expected command to time out");
        assert!(timed.exit_code.is_none());

        // Env allowlist test: explicit env var passes, arbitrary secret env does not
        let mut extra = BTreeMap::new();
        extra.insert("RIFFTY_AB_TEST_VAR".to_string(), "allowed_value".to_string());
        let env_out = run_sandboxed_command(
            &tmp,
            "printf '%s|%s' \"$RIFFTY_AB_TEST_VAR\" \"${AWS_SECRET_ACCESS_KEY:-EMPTY}\"",
            10,
            &extra,
            &SandboxMode::None,
        )
        .expect("env command should run");
        assert_eq!(env_out.exit_code, Some(0));
        assert_eq!(env_out.stdout, "allowed_value|EMPTY");
    }
}
