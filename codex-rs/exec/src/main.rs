//! Entry-point for the `codex-exec` binary.
//!
//! When this CLI is invoked normally, it parses the standard `codex-exec` CLI
//! options and launches the non-interactive Codex agent. However, if it is
//! invoked with arg0 as `codex-linux-sandbox`, we instead treat the invocation
//! as a request to run the logic for the standalone `codex-linux-sandbox`
//! executable (i.e., parse any -s args and then run a *sandboxed* command under
//! Landlock + seccomp.
//!
//! This allows us to ship a completely separate set of functionality as part
//! of the `codex-exec` binary.
use clap::Parser;
use codex_arg0::Arg0DispatchPaths;
use codex_arg0::SilentExit;
use codex_arg0::arg0_dispatch_or_else;
use codex_arg0::exit_code_from_result;
use codex_exec::Cli;
use codex_exec::run_main;
use codex_utils_cli::CliConfigOverrides;

#[derive(Parser, Debug)]
struct TopCli {
    #[clap(flatten)]
    config_overrides: CliConfigOverrides,

    #[clap(flatten)]
    inner: Cli,
}

fn main() -> std::process::ExitCode {
    let result = arg0_dispatch_or_else(|arg0_paths: Arg0DispatchPaths| async move {
        let top_cli = match TopCli::try_parse() {
            Ok(top_cli) => top_cli,
            Err(err) => {
                // Render clap's message exactly as `parse()` would, then return
                // the same exit code through the silent path so the arg0 guard
                // is dropped instead of leaked.
                let code = u8::try_from(err.exit_code()).unwrap_or(1);
                let _ = err.print();
                return Err(SilentExit::new(code).into());
            }
        };
        // Merge root-level overrides into inner CLI struct so downstream logic remains unchanged.
        let mut inner = top_cli.inner;
        inner
            .config_overrides
            .prepend_root_overrides(top_cli.config_overrides);

        run_main(inner, arg0_paths).await?;
        Ok(())
    });
    exit_code_from_result(result)
}

#[cfg(test)]
#[path = "main_tests.rs"]
mod tests;
