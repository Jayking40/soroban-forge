use crate::cli::DoctorArgs;
use anyhow::Result;
use std::fs;
use std::io;
use std::path::Path;
use std::process::{Command, Output};

const WASM_TARGET: &str = "wasm32v1-none";

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct CheckResult {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
    pub fix_hint: Option<&'static str>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct CommandOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

pub trait CommandRunner {
    fn output(&self, program: &str, args: &[&str]) -> io::Result<CommandOutput>;
}

struct SystemCommandRunner;

impl CommandRunner for SystemCommandRunner {
    fn output(&self, program: &str, args: &[&str]) -> io::Result<CommandOutput> {
        let output = Command::new(program).args(args).output()?;
        Ok(command_output(output))
    }
}

fn command_output(output: Output) -> CommandOutput {
    CommandOutput {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

pub fn run(args: DoctorArgs) -> Result<()> {
    let results = collect_checks(&SystemCommandRunner, args.wasm.as_deref());
    print_report(&results);
    if results.iter().all(|result| result.ok) {
        Ok(())
    } else {
        anyhow::bail!("environment preflight failed")
    }
}

pub fn collect_checks(runner: &impl CommandRunner, wasm: Option<&Path>) -> Vec<CheckResult> {
    let mut results = vec![check_versioned_tool(runner, "stellar", &["--version"])];
    results.push(check_versioned_tool(runner, "cargo", &["--version"]));
    results.push(check_wasm_target(runner));
    if let Some(path) = wasm {
        results.push(check_wasm_artifact(path));
    }
    results
}

fn check_versioned_tool(
    runner: &impl CommandRunner,
    name: &'static str,
    args: &[&str],
) -> CheckResult {
    match runner.output(name, args) {
        Ok(output) => {
            let version = output
                .stdout
                .lines()
                .chain(output.stderr.lines())
                .map(str::trim)
                .find(|line| !line.is_empty());
            CheckResult {
                name,
                ok: true,
                detail: version
                    .map(str::to_owned)
                    .unwrap_or_else(|| "version unknown".to_owned()),
                fix_hint: None,
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => CheckResult {
            name,
            ok: false,
            detail: "not found on PATH".to_owned(),
            fix_hint: Some(if name == "stellar" {
                "cargo install soroban-cli"
            } else {
                "install Rust and Cargo"
            }),
        },
        Err(_) => CheckResult {
            name,
            ok: true,
            detail: "version unknown".to_owned(),
            fix_hint: None,
        },
    }
}

fn check_wasm_target(runner: &impl CommandRunner) -> CheckResult {
    match runner.output("rustup", &["target", "list", "--installed"]) {
        Ok(output) if output.success => {
            let installed = output.stdout.lines().any(|line| line.trim() == WASM_TARGET);
            CheckResult {
                name: WASM_TARGET,
                ok: installed,
                detail: if installed {
                    "installed".to_owned()
                } else {
                    "not installed".to_owned()
                },
                fix_hint: (!installed).then_some("rustup target add wasm32v1-none"),
            }
        }
        Ok(_) | Err(_) => CheckResult {
            name: WASM_TARGET,
            ok: false,
            detail: "could not verify rustup target list".to_owned(),
            fix_hint: Some("rustup target add wasm32v1-none"),
        },
    }
}

fn check_wasm_artifact(path: &Path) -> CheckResult {
    let name = "WASM artifact";
    match fs::metadata(path) {
        Ok(metadata) if !metadata.is_file() => CheckResult {
            name,
            ok: false,
            detail: format!("{} is not a regular file", path.display()),
            fix_hint: Some("cargo build --release --target wasm32v1-none"),
        },
        Ok(metadata) if metadata.len() == 0 => CheckResult {
            name,
            ok: false,
            detail: format!("{} is empty", path.display()),
            fix_hint: Some("cargo build --release --target wasm32v1-none"),
        },
        Ok(_) => CheckResult {
            name,
            ok: true,
            detail: format!("{} is a non-empty file", path.display()),
            fix_hint: None,
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => CheckResult {
            name,
            ok: false,
            detail: format!("{} does not exist", path.display()),
            fix_hint: Some("cargo build --release --target wasm32v1-none"),
        },
        Err(error) => CheckResult {
            name,
            ok: false,
            detail: format!("could not inspect {}: {error}", path.display()),
            fix_hint: Some("cargo build --release --target wasm32v1-none"),
        },
    }
}

fn print_report(results: &[CheckResult]) {
    println!("Check                    Status  Details");
    println!("-----------------------  ------  ------------------------------");
    for result in results {
        println!(
            "{:<23}  {:<6}  {}",
            result.name,
            if result.ok { "PASS" } else { "FAIL" },
            result.detail
        );
        if let Some(fix) = result.fix_hint {
            println!("  fix: {fix}");
        }
    }
    let failed: Vec<_> = results
        .iter()
        .filter(|result| !result.ok)
        .map(|result| result.name)
        .collect();
    if failed.is_empty() {
        println!("Summary: all checks passed");
    } else {
        println!("Summary: failed checks: {}", failed.join(", "));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::fs;

    struct SequenceRunner(std::cell::RefCell<VecDeque<io::Result<CommandOutput>>>);

    impl CommandRunner for SequenceRunner {
        fn output(&self, _program: &str, _args: &[&str]) -> io::Result<CommandOutput> {
            self.0.borrow_mut().pop_front().expect("response available")
        }
    }

    fn output(stdout: &str, success: bool) -> io::Result<CommandOutput> {
        Ok(CommandOutput {
            success,
            stdout: stdout.to_owned(),
            stderr: String::new(),
        })
    }

    #[test]
    fn checks_run_in_required_order_and_all_pass() {
        let runner = SequenceRunner(std::cell::RefCell::new(
            vec![
                output("stellar 1.0", false),
                output("cargo 1.0", true),
                output("x86_64-unknown-linux-gnu\nwasm32v1-none\n", true),
            ]
            .into(),
        ));
        let results = collect_checks(&runner, None);
        assert_eq!(
            results.iter().map(|result| result.name).collect::<Vec<_>>(),
            vec!["stellar", "cargo", "wasm32v1-none"]
        );
        assert!(results.iter().all(|result| result.ok));
        assert_eq!(results[0].detail, "stellar 1.0");
    }

    #[test]
    fn missing_and_failed_checks_are_reported_without_short_circuiting() {
        let runner = SequenceRunner(std::cell::RefCell::new(
            vec![
                Err(io::Error::new(io::ErrorKind::NotFound, "missing")),
                output("", false),
                output("", false),
            ]
            .into(),
        ));
        let results = collect_checks(&runner, None);
        assert_eq!(results.len(), 3);
        assert!(!results[0].ok);
        assert!(results[1].ok);
        assert!(!results[2].ok);
        assert_eq!(results[2].detail, "could not verify rustup target list");
    }

    #[test]
    fn optional_artifact_distinguishes_missing_empty_directory_and_valid_file() {
        let root = std::env::temp_dir().join(format!("soroban-doctor-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let missing = check_wasm_artifact(&root.join("missing.wasm"));
        assert!(missing.detail.contains("does not exist"));
        let empty = root.join("empty.wasm");
        fs::write(&empty, []).unwrap();
        assert!(check_wasm_artifact(&empty).detail.contains("empty"));
        let directory = root.join("directory.wasm");
        fs::create_dir(&directory).unwrap();
        assert!(check_wasm_artifact(&directory)
            .detail
            .contains("regular file"));
        let valid = root.join("valid.wasm");
        fs::write(&valid, [1_u8]).unwrap();
        assert!(check_wasm_artifact(&valid).ok);
        fs::remove_dir_all(root).unwrap();
    }
}
