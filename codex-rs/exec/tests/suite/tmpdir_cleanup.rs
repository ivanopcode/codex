#![allow(clippy::unwrap_used)]

use std::path::Path;

use core_test_support::responses;

/// List `.tmp*` entries directly under `dir` (tempfile's default prefix).
fn tmp_entries(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(read) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in read.flatten() {
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
fn leaked_arg0_dirs(codex_home: &Path) -> Vec<String> {
    let arg0_root = codex_home.join("tmp").join("arg0");
    let Ok(read) = std::fs::read_dir(&arg0_root) else {
        return Vec::new();
    };
    let mut out: Vec<String> = read
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("codex-arg0"))
        .collect();
    out.sort();
    out
}

struct IsolatedExec {
    /// Isolated TMPDIR for the child process. Held so it is removed on drop.
    _tmpdir_guard: tempfile::TempDir,
    tmpdir: std::path::PathBuf,
    home: std::path::PathBuf,
    cwd: std::path::PathBuf,
}

impl IsolatedExec {
    fn new() -> anyhow::Result<Self> {
        let tmpdir_guard = tempfile::TempDir::new()?;
        let tmpdir = tmpdir_guard.path().to_path_buf();
        let home = tmpdir.join("home");
        let cwd = tmpdir.join("cwd");
        std::fs::create_dir_all(&home)?;
        std::fs::create_dir_all(&cwd)?;
        Ok(Self {
            _tmpdir_guard: tmpdir_guard,
            tmpdir,
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
            .env("TMP", &self.tmpdir);
        cmd
    }

    fn assert_no_leaks(&self) {
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
    test.cmd()
        .arg("--skip-git-repo-check")
        .arg("-c")
        .arg(format!("openai_base_url={}", serde_json::to_string(&base)?))
        .arg("Reply with exactly: ok")
        .assert()
        .success();

    test.assert_no_leaks();
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
    test.cmd()
        .arg("--skip-git-repo-check")
        .arg("-c")
        .arg(format!("openai_base_url={}", serde_json::to_string(&base)?))
        .arg("tell me something")
        .assert()
        .code(1);

    test.assert_no_leaks();
    Ok(())
}

/// Early stdin error exits non-zero without leaking temporary directories.
///
/// Empty piped stdin fails before any network use. The old
/// `read_prompt_from_stdin` called `process::exit(1)`, skipping destructors.
#[test]
fn exec_empty_stdin_leaves_no_tmpdirs() -> anyhow::Result<()> {
    let test = IsolatedExec::new()?;

    test.cmd()
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(&test.cwd)
        .write_stdin("")
        .assert()
        .code(1);

    test.assert_no_leaks();
    Ok(())
}
