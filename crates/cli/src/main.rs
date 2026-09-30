//! Soroban Forge developer CLI.
//!
//! ```text
//! soroban-forge build
//! soroban-forge build --wasm --check-size
//! soroban-forge test --package soroban-forge-escrow
//! soroban-forge lint --fix
//! soroban-forge deploy path/to/escrow.wasm --network testnet
//! soroban-forge verify --wasm path/to/contract.wasm
//! soroban-forge verify --expected <sha256>
//! soroban-forge verify --manifest provenance-manifest.json
//! soroban-forge doctor --wasm target/wasm32v1-none/release/contract.wasm
//! ```

mod cli;
mod commands;

use clap::{Parser, Subcommand};
use cli::{
    BuildArgs, DeployArgs, DoctorArgs, EventsArgs, InvokeArgs, LintArgs, NewArgs, TestArgs,
    VerifyArgs,
};

#[derive(Parser, Debug)]
#[command(
    name = "soroban-forge",
    about = "Developer CLI for Soroban Forge",
    version,
    author
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    Build(BuildArgs),
    Lint(LintArgs),
    Test(TestArgs),
    Deploy(DeployArgs),
    New(NewArgs),
    Verify(VerifyArgs),
    Invoke(InvokeArgs),
    Events(EventsArgs),
    Doctor(DoctorArgs),
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    match cli.command {
        Commands::Build(args) => commands::build::run(args)?,
        Commands::Lint(args) => commands::lint::run(args)?,
        Commands::Test(args) => commands::test::run(args)?,
        Commands::Deploy(args) => commands::deploy::run(args)?,
        Commands::New(args) => commands::new::run(args)?,
        Commands::Verify(args) => commands::verify::run(args)?,
        Commands::Invoke(args) => commands::invoke::run(args)?,
        Commands::Events(args) => commands::events::run(args)?,
        Commands::Doctor(args) => commands::doctor::run(args)?,
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Cli, Commands};
    use clap::{CommandFactory, Parser};

    #[test]
    fn build_wasm_and_size_flags_parse() {
        let cli = Cli::try_parse_from([
            "soroban-forge",
            "build",
            "--wasm",
            "--check-size",
            "--package",
            "soroban-forge-escrow",
        ])
        .unwrap();

        let Commands::Build(args) = cli.command else {
            panic!("expected build command");
        };
        assert!(args.wasm);
        assert!(args.check_size);
        assert_eq!(args.package.as_deref(), Some("soroban-forge-escrow"));
    }

    #[test]
    fn build_help_documents_wasm_and_size_flags() {
        let mut command = Cli::command();
        let help = command
            .find_subcommand_mut("build")
            .expect("build subcommand must exist")
            .render_long_help()
            .to_string();
        assert!(help.contains("--wasm"));
        assert!(help.contains("--check-size"));
    }

    #[test]
    fn verify_flags_parse() {
        let cli = Cli::try_parse_from([
            "soroban-forge",
            "verify",
            "--wasm",
            "target/wasm32v1-none/release/soroban_forge_escrow.wasm",
            "--expected",
            &"a".repeat(64),
        ])
        .unwrap();

        let Commands::Verify(args) = cli.command else {
            panic!("expected verify command");
        };
        assert_eq!(
            args.wasm.as_deref(),
            Some("target/wasm32v1-none/release/soroban_forge_escrow.wasm")
        );
        assert_eq!(args.expected.as_deref(), Some("a".repeat(64).as_str()));
        assert_eq!(args.manifest, "provenance-manifest.json");
    }

    #[test]
    fn verify_manifest_defaults_to_provenance_manifest() {
        let cli = Cli::try_parse_from(["soroban-forge", "verify", "--wasm", "x.wasm"]).unwrap();
        let Commands::Verify(args) = cli.command else {
            panic!("expected verify command");
        };
        assert_eq!(args.manifest, "provenance-manifest.json");
    }

    #[test]
    fn verify_help_documents_flags() {
        let mut command = Cli::command();
        let help = command
            .find_subcommand_mut("verify")
            .expect("verify subcommand must exist")
            .render_long_help()
            .to_string();
        assert!(help.contains("--wasm"));
        assert!(help.contains("--expected"));
        assert!(help.contains("--manifest"));
    }
}
