use std::path::PathBuf;
use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    let workspace_root = std::env::var("RIFFTY_WORKSPACE_ROOT")
        .map(PathBuf::from)
        .or_else(|_| std::env::current_dir())
        .unwrap_or_else(|_| PathBuf::from("."));

    let rules_path = std::env::var("RIFFTY_RULES_PATH")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from);

    if let Err(err) = riffty_mcp::run_stdio_server(workspace_root, rules_path).await {
        eprintln!("riffty_mcp error: {err}");
        return ExitCode::from(2);
    }
    ExitCode::SUCCESS
}

