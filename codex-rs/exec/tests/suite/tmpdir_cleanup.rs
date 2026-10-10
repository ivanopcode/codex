#![allow(clippy::unwrap_used)]

use std::path::Path;

use core_test_support::responses;
use pretty_assertions::assert_eq;

/// List `.tmp*` entries directly under `dir` (tempfile's default prefix).
///
/// Fails closed: any read error panics instead of reporting "clean", so a
/// failed or partial read is never mistaken for absence of leaks.
fn tmp_entries(dir: &Path) -> Vec<String> {
    let read = std::fs::read_dir(dir)
        .unwrap_or_else(|err| panic!("failed to read TMPDIR {}: {err}", dir.display()));
    let mut out = Vec::new();
    for entry in read {
        let entry =
            entry.unwrap_or_else(|err| panic!("failed to read entry in {}: {err}", dir.display()));
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(".tmp") {
            out.push(name);
        }
    }
    out.sort();
    out
}

/// List leaked arg0 session dirs under `codex_home/tmp/arg0`.
///
/// The arg0 guard creates one `codex-arg0*` subdir per process and removes it
/// on drop. A `process::exit` before drop leaves it behind.
///
/// Fails closed: any read error (including a missing root, which means alias
/// setup never ran) panics instead of reporting "clean".
fn leaked_arg0_dirs(codex_home: &Path) -> Vec<String> {
    let arg0_root = codex_home.join("tmp").join("arg0");
    let read = std::fs::read_dir(&arg0_root)
        .unwrap_or_else(|err| panic!("failed to read arg0 root {}: {err}", arg0_root.display()));
    let mut out = Vec::new();
    for entry in read {
        let entry = entry
            .unwrap_or_else(|err| panic!("failed to read entry in {}: {err}", arg0_root.display()));
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("codex-arg0") {
            out.push(name);
        }
    }
    out.sort();
    out
}

/// Assert that arg0 alias setup actually ran for a child using `codex_home`.
///
/// Production permits setup failure and proceeds with no guard; a "no leak"
/// assertion without this precondition would pass vacuously when setup was
/// refused (for example a release build with CODEX_HOME under TMPDIR, which
/// arg0 explicitly refuses). The setup creates `CODEX_HOME/tmp/arg0` and
/// prints a warning on any failure, so an existing root plus a warning-free
/// stderr proves the guard existed during the run.
fn assert_arg0_setup_exercised(codex_home: &Path, stderr: &str) {
    let arg0_root = codex_home.join("tmp").join("arg0");
    assert!(
        arg0_root.is_dir(),
        "arg0 alias setup was not exercised: {} is not a directory (stderr: {stderr})",
        arg0_root.display()
    );
    assert!(
        !stderr.contains("could not create PATH aliases"),
        "arg0 alias setup failed (stderr: {stderr})"
    );
}

struct IsolatedExec {
    /// Isolated TMPDIR for the child process. Held so it is removed on drop.
    _tmpdir_guard: tempfile::TempDir,
    tmpdir: std::path::PathBuf,
    /// Separate CODEX_HOME outside the isolated TMPDIR, held for cleanup.
    ///
    /// Release builds refuse arg0 alias setup when CODEX_HOME sits under the
    /// system temp root, so the home lives in its own sibling temp dir. This
    /// keeps the arg0 path exercised in both debug and release builds.
    _home_guard: tempfile::TempDir,
    home: std::path::PathBuf,
    cwd: std::path::PathBuf,
}

impl IsolatedExec {
    fn new() -> anyhow::Result<Self> {
        let tmpdir_guard = tempfile::TempDir::new()?;
        let tmpdir = tmpdir_guard.path().to_path_buf();
        let home_guard = tempfile::TempDir::new()?;
        let home = home_guard.path().join("home");
        let cwd = tmpdir.join("cwd");
        std::fs::create_dir_all(&home)?;
        std::fs::create_dir_all(&cwd)?;
        Ok(Self {
            _tmpdir_guard: tmpdir_guard,
            tmpdir,
            _home_guard: home_guard,
            home,
            cwd,
        })
    }

    fn cmd(&self) -> assert_cmd::Command {
        let mut cmd = assert_cmd::Command::new(
            codex_utils_cargo_bin::cargo_bin("codex-exec").expect("codex-exec binary"),
        );
        cmd.current_dir(&self.cwd)
            .env("CODEX_HOME", &self.home)
            .env("CODEX_SQLITE_HOME", &self.home)
            .env(codex_login::CODEX_API_KEY_ENV_VAR, "dummy")
            // Isolate every temp-dir env var so tempfile and std::env::temp_dir
            // agree on the sandbox on all platforms.
            .env("TMPDIR", &self.tmpdir)
            .env("TEMP", &self.tmpdir)
            .env("TMP", &self.tmpdir)
            // Suppress tracing output so full-stderr assertions stay exact.
            .env("RUST_LOG", "off");
        cmd
    }

    fn assert_no_leaks(&self, stderr: &str) {
        assert_arg0_setup_exercised(&self.home, stderr);
        let tmp = tmp_entries(&self.tmpdir);
        assert!(
            tmp.is_empty(),
            "isolated TMPDIR leaked .tmp* entries: {tmp:?}"
        );
        let arg0 = leaked_arg0_dirs(&self.home);
        assert!(
            arg0.is_empty(),
            "CODEX_HOME/tmp/arg0 leaked session dirs: {arg0:?}"
        );
    }
}

/// Success path leaves no temporary directories behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_success_leaves_no_tmpdirs() -> anyhow::Result<()> {
    let test = IsolatedExec::new()?;
    let server = responses::start_mock_server().await;
    let body = responses::sse(vec![
        responses::ev_response_created("resp_ok"),
        responses::ev_assistant_message("msg_ok", "ok"),
        responses::ev_completed("resp_ok"),
    ]);
    responses::mount_sse_once(&server, body).await;

    let base = format!("{}/v1", server.uri());
    let assert = test
        .cmd()
        .arg("--skip-git-repo-check")
        .arg("-c")
        .arg(format!("openai_base_url={}", serde_json::to_string(&base)?))
        .arg("Reply with exactly: ok")
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();

    test.assert_no_leaks(&stderr);
    Ok(())
}

/// Server-error path exits non-zero and still cleans up temporary directories.
///
/// Before the fix this used `process::exit(1)` after shutdown, skipping the
/// arg0 temporary-directory guard and leaking one `codex-arg0*` dir per run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_server_error_leaves_no_tmpdirs() -> anyhow::Result<()> {
    let test = IsolatedExec::new()?;
    let server = responses::start_mock_server().await;
    let body = responses::sse(vec![serde_json::json!({
        "type": "response.failed",
        "response": {
            "id": "resp_err",
            "error": {"code": "rate_limit_exceeded", "message": "synthetic server error"}
        }
    })]);
    responses::mount_sse_once(&server, body).await;

    let base = format!("{}/v1", server.uri());
    let assert = test
        .cmd()
        .arg("--skip-git-repo-check")
        .arg("-c")
        .arg(format!("openai_base_url={}", serde_json::to_string(&base)?))
        .arg("tell me something")
        .assert()
        .code(1);
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();

    test.assert_no_leaks(&stderr);
    Ok(())
}

/// Early stdin error exits non-zero without leaking temporary directories.
///
/// Empty piped stdin fails before any network use. The old
/// `read_prompt_from_stdin` called `process::exit(1)`, skipping destructors.
#[test]
fn exec_empty_stdin_leaves_no_tmpdirs() -> anyhow::Result<()> {
    let test = IsolatedExec::new()?;

    let assert = test
        .cmd()
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(&test.cwd)
        .write_stdin("")
        .assert()
        .code(1);
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();

    test.assert_no_leaks(&stderr);
    Ok(())
}

/// Empty stdin preserves the exact pre-fix stderr bytes (no `Error: ` prefix).
///
/// The diagnostic was previously printed with `eprintln!` followed by
/// `process::exit(1)`; returning through the silent path must keep the same
/// bytes and ordering while letting the guards drop.
#[test]
fn exec_empty_stdin_preserves_stderr() -> anyhow::Result<()> {
    let test = IsolatedExec::new()?;

    let assert = test
        .cmd()
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(&test.cwd)
        .write_stdin("")
        .assert()
        .code(1);
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert_eq!(
        stderr.as_str(),
        "Reading prompt from stdin...\nNo prompt provided via stdin.\n"
    );

    test.assert_no_leaks(&stderr);
    Ok(())
}

/// A `-c` override parse error preserves the exact pre-fix stderr bytes.
///
/// The invalid override fails before any other output, so the full stderr is
/// a single deterministic line.
#[test]
fn exec_config_error_preserves_stderr() -> anyhow::Result<()> {
    let test = IsolatedExec::new()?;

    let assert = test
        .cmd()
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(&test.cwd)
        .arg("-c")
        .arg("invalid")
        .arg("hello")
        .write_stdin("")
        .assert()
        .code(1);
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert_eq!(
        stderr.as_str(),
        "Error parsing -c overrides: Invalid override (missing '='): invalid\n"
    );

    test.assert_no_leaks(&stderr);
    Ok(())
}

/// A server failure in JSON mode emits no extra diagnostic beyond stdin.
///
/// In `--experimental-json` mode all turn events go to stdout; the pre-fix
/// code exited silently after `print_final_output`, so the fixed silent path
/// must also emit nothing extra. The prompt argument plus piped empty stdin
/// deterministically produces the single "Reading additional input" line on
/// stderr. The stdout assertion proves the failure path was actually
/// exercised rather than failing earlier.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_server_error_preserves_stderr() -> anyhow::Result<()> {
    let test = IsolatedExec::new()?;
    let server = responses::start_mock_server().await;
    let body = responses::sse(vec![serde_json::json!({
        "type": "response.failed",
        "response": {
            "id": "resp_err_json",
            "error": {"code": "rate_limit_exceeded", "message": "synthetic server error"}
        }
    })]);
    responses::mount_sse_once(&server, body).await;

    let base = format!("{}/v1", server.uri());
    let assert = test
        .cmd()
        .arg("--skip-git-repo-check")
        .arg("-c")
        .arg(format!("openai_base_url={}", serde_json::to_string(&base)?))
        .arg("--experimental-json")
        .arg("tell me something")
        .write_stdin("")
        .assert()
        .code(1);
    let output = assert.get_output();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert_eq!(stderr.as_str(), "Reading additional input from stdin...\n");
    assert!(
        stdout.contains("turn.failed"),
        "expected turn.failed in stdout, got: {stdout}"
    );

    test.assert_no_leaks(&stderr);
    Ok(())
}

/// Invalid CLI flags exit 2 through clap's rendering without leaking.
///
/// Clap's `parse()` previously exited the process before the arg0 guard could
/// drop; `try_parse()` plus explicit rendering keeps the same output and code
/// while letting the guard drop.
#[test]
fn exec_invalid_args_leave_no_tmpdirs() -> anyhow::Result<()> {
    let test = IsolatedExec::new()?;

    let assert = test
        .cmd()
        .arg("--definitely-invalid-option")
        .assert()
        .code(2);
    let output = assert.get_output();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert_eq!(stdout.as_str(), "");
    assert!(
        stderr.contains("--definitely-invalid-option"),
        "expected clap error to mention the flag, got: {stderr}"
    );
    assert!(
        stderr.contains("error"),
        "expected clap error prefix, got: {stderr}"
    );

    test.assert_no_leaks(&stderr);
    Ok(())
}

/// `--help` exits 0 through clap's rendering without leaking the arg0 guard.
#[test]
fn exec_help_leaves_no_tmpdirs() -> anyhow::Result<()> {
    let test = IsolatedExec::new()?;

    let assert = test.cmd().arg("--help").assert().code(0);
    let output = assert.get_output();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert_eq!(stderr.as_str(), "");
    assert!(
        stdout.contains("Usage"),
        "expected clap help in stdout, got: {stdout}"
    );

    test.assert_no_leaks(&stderr);
    Ok(())
}

/// The TMPDIR oracle fails closed instead of reporting an unreadable root.
#[test]
#[should_panic(expected = "failed to read TMPDIR")]
fn leak_scan_rejects_unreadable_tmp_root() {
    let file = tempfile::NamedTempFile::new().expect("temp file");
    let _ = tmp_entries(file.path());
}

/// The arg0 oracle fails closed when alias setup never created its root.
#[test]
#[should_panic(expected = "failed to read arg0 root")]
fn leak_scan_rejects_missing_arg0_root() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let _ = leaked_arg0_dirs(dir.path());
}

/// Ordinary errors must not panic when the terminal stderr write fails.
///
/// `codex-exec --worktree --ephemeral` returns an ordinary anyhow error
/// before any other output. The base `fn main() -> anyhow::Result<()>` printed
/// it via `Termination`, which ignores write failures; the rev2 `eprintln!`
/// panicked (exit 101). Delegating to `Termination` preserves exit 1 and
/// cleanup. Skips where `/dev/full` is absent.
#[test]
fn exec_ordinary_error_stderr_failure_preserves_exit_code() -> anyhow::Result<()> {
    let dev_full = Path::new("/dev/full");
    if !dev_full.exists() {
        eprintln!(
            "SKIP: /dev/full does not exist on this platform; failing-stderr path not exercised"
        );
        return Ok(());
    }
    let test = IsolatedExec::new()?;
    let stderr_file = std::fs::File::options().write(true).open(dev_full)?;
    let bin = codex_utils_cargo_bin::cargo_bin("codex-exec")?;
    let mut cmd = std::process::Command::new(bin);
    cmd.current_dir(&test.cwd)
        .env("CODEX_HOME", &test.home)
        .env("CODEX_SQLITE_HOME", &test.home)
        .env(codex_login::CODEX_API_KEY_ENV_VAR, "dummy")
        .env("TMPDIR", &test.tmpdir)
        .env("TEMP", &test.tmpdir)
        .env("TMP", &test.tmpdir)
        .env("RUST_LOG", "off")
        .arg("--skip-git-repo-check")
        .arg("--worktree")
        .arg("--ephemeral")
        .arg("hello")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::from(stderr_file));
    let output = cmd.output()?;
    assert_eq!(
        output.status.code(),
        Some(1),
        "expected exit 1 (not panic 101) with failing stderr"
    );
    // Stderr went to /dev/full, so the warning-free check is unavailable; an
    // existing arg0 root still proves the CODEX_HOME-under-TMPDIR refusal path
    // was not hit (that path returns before creating the root).
    let arg0_root = test.home.join("tmp").join("arg0");
    assert!(
        arg0_root.is_dir(),
        "arg0 alias setup was not exercised: {} is not a directory",
        arg0_root.display()
    );
    let tmp = tmp_entries(&test.tmpdir);
    assert!(
        tmp.is_empty(),
        "isolated TMPDIR leaked .tmp* entries: {tmp:?}"
    );
    let arg0 = leaked_arg0_dirs(&test.home);
    assert!(
        arg0.is_empty(),
        "CODEX_HOME/tmp/arg0 leaked session dirs: {arg0:?}"
    );
    Ok(())
}
