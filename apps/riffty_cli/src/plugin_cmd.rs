use clap::{Args, Subcommand, ValueEnum};
use riffty_adapter_plugin::{
    cached_java_sidecar_dir, clean_java_sidecar_cache, current_target_triple,
    discover_java_sidecar_bundle, hydrate_java_sidecar, verify_jar_sha256,
};

#[derive(Debug, Args)]
pub struct PluginArgs {
    #[command(subcommand)]
    pub command: PluginSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum PluginSubcommand {
    /// Pre-hydrate a verified self-contained language plugin (for offline / air-gapped CI)
    Install(PluginInstallArgs),
    /// Show status and SHA-256 verification of bundled / cached language plugins
    Status,
    /// Remove cached lazily hydrated language plugins
    Clean,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum PluginTarget {
    Java,
}

#[derive(Debug, Args)]
pub struct PluginInstallArgs {
    /// Plugin to install (`java`)
    #[arg(value_enum)]
    pub target: PluginTarget,

    /// Re-download and re-verify even if already cached
    #[arg(long)]
    pub force: bool,
}

pub fn run_plugin(args: PluginArgs) -> Result<(), String> {
    match args.command {
        PluginSubcommand::Install(install) => match install.target {
            PluginTarget::Java => {
                let version = env!("CARGO_PKG_VERSION");
                let triple = current_target_triple()?;
                eprintln!("riffty: hydrating verified riffty-java v{version} ({triple})...");
                let bundle = hydrate_java_sidecar(version, install.force)?;
                println!("Installed riffty-java v{version} ({triple})");
                println!("  java_bin:   {}", bundle.java_bin.display());
                println!("  runner_jar: {}", bundle.runner_jar.display());
                println!("  sha256:     {} (VERIFIED)", bundle.expected_sha256);
                if let Some((ref aot, ref sha)) = bundle.aot_cache {
                    println!(
                        "  aot_cache:  {} ({})",
                        aot.display(),
                        &sha[..12.min(sha.len())]
                    );
                }
                Ok(())
            }
        },
        PluginSubcommand::Status => {
            let version = env!("CARGO_PKG_VERSION");
            println!("riffty v{version} language runtimes:");
            println!("  python (Tier 1+2): built-in pure Rust (ruff-ast + ty)");
            println!("  jsts   (Tier 1+2): built-in pure Rust (oxc + tsrs)");
            println!("  java   (Tier 1):   built-in pure Rust (tree-sitter-java)");
            match discover_java_sidecar_bundle() {
                Some(bundle) => {
                    let integrity =
                        match verify_jar_sha256(&bundle.runner_jar, &bundle.expected_sha256) {
                            Ok(()) => "SHA-256 OK",
                            Err(_) => "SHA-256 MISMATCH",
                        };
                    println!("  java   (Tier 2):   READY ({integrity})");
                    println!("    java_bin:   {}", bundle.java_bin.display());
                    println!("    runner_jar: {}", bundle.runner_jar.display());
                    if let Some((ref aot, _)) = bundle.aot_cache {
                        println!("    aot_cache:  {}", aot.display());
                    }
                }
                None => {
                    let cache_hint = cached_java_sidecar_dir(version)
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "~/.cache/riffty/plugins/riffty-java".to_string());
                    println!(
                        "  java   (Tier 2):   NOT HYDRATED (auto-downloads on first .java check, or run `riffty plugin install java`)"
                    );
                    println!("    cache_dir:  {cache_hint}");
                }
            }
            Ok(())
        }
        PluginSubcommand::Clean => {
            match clean_java_sidecar_cache()? {
                Some(removed) => println!("Removed cached plugins at {}", removed.display()),
                None => println!("No cached plugins to remove."),
            }
            Ok(())
        }
    }
}
