use std::os::unix::process::ExitStatusExt;
use std::process::{Command, ExitStatus, Stdio};

use claude_gpt::cli::{BridgeCommand, parse_from};
use claude_gpt::codex_transport::CodexTransport;
use claude_gpt::compatibility::Compatibility;
use claude_gpt::doctor::{DoctorMode, run as run_doctor};
use claude_gpt::error::{BridgeError, Result};
use claude_gpt::install::{install_from, repair_from, uninstall_at};
use claude_gpt::launcher;
use claude_gpt::mcp;
use claude_gpt::paths::AppPaths;
use claude_gpt::usage::render_usage;

#[tokio::main]
async fn main() {
    let code = match execute().await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("claude-gpt: {error}");
            1
        }
    };
    std::process::exit(code);
}

async fn execute() -> Result<i32> {
    match parse_from(std::env::args_os())? {
        BridgeCommand::Launch(arguments) => launcher::run(arguments).await.map(exit_code),
        BridgeCommand::Install => {
            let paths = AppPaths::resolve()?;
            preflight_install(&paths).await?;
            let current = std::env::current_exe().map_err(|source| BridgeError::Process {
                program: "claude-gpt".into(),
                source,
            })?;
            install_from(&paths, &current)?;
            println!("Installed {}", paths.install_bin.display());
            println!("VS Code will now start the GPT bridge automatically.");
            Ok(0)
        }
        BridgeCommand::Repair => {
            let paths = AppPaths::resolve()?;
            preflight_install(&paths).await?;
            let current = std::env::current_exe().map_err(|source| BridgeError::Process {
                program: "claude-gpt".into(),
                source,
            })?;
            repair_from(&paths, &current)?;
            println!("Repaired claude-gpt integration.");
            Ok(0)
        }
        BridgeCommand::Uninstall => {
            let paths = AppPaths::resolve()?;
            uninstall_at(&paths)?;
            println!("Uninstalled claude-gpt integration; configuration backup was retained.");
            Ok(0)
        }
        BridgeCommand::Doctor { live } => {
            let paths = AppPaths::resolve()?;
            let mode = if live {
                DoctorMode::Live
            } else {
                DoctorMode::Offline
            };
            print!("{}", run_doctor(&paths, mode).await?);
            Ok(0)
        }
        BridgeCommand::Login => {
            let paths = AppPaths::resolve()?;
            Compatibility::embedded()?.verify(&paths)?;
            let status = Command::new(&paths.codex)
                .arg("login")
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .status()
                .map_err(|source| BridgeError::Process {
                    program: paths.codex,
                    source,
                })?;
            Ok(exit_code(status))
        }
        BridgeCommand::Usage => {
            let paths = AppPaths::resolve()?;
            Compatibility::embedded()?.verify(&paths)?;
            let transport = CodexTransport::connect(&paths).await?;
            println!("{}", render_usage(&transport.rate_limits().await?));
            Ok(0)
        }
        BridgeCommand::Mcp => {
            let paths = AppPaths::resolve()?;
            Compatibility::embedded()?.verify(&paths)?;
            mcp::run(paths).await?;
            Ok(0)
        }
    }
}

async fn preflight_install(paths: &AppPaths) -> Result<()> {
    Compatibility::embedded()?.verify(paths)?;
    CodexTransport::connect(paths)
        .await?
        .refresh_catalog()
        .await?;
    Ok(())
}

fn exit_code(status: ExitStatus) -> i32 {
    status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(1)
}
