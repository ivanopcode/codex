#![allow(clippy::unwrap_used)]

use std::path::Path;

/// List leaked arg0 session dirs under `codex_home/tmp/arg0`, failing closed.
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

/// Prove arg0 alias setup ran; a missing root means the test is vacuous.
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

struct IsolatedCli {
    _tmpdir_guard: tempfile::TempDir,
    tmpdir: std::path::PathBuf,
    _home_guard: tempfile::TempDir,
    home: std::path::PathBuf,
}

impl IsolatedCli {
    fn new() -> anyhow::Result<Self> {
        let tmpdir_guard = tempfile::TempDir::new()?;
        let tmpdir = tmpdir_guard.path().to_path_buf();
        let home_guard = tempfile::TempDir::new()?;
        let home = home_guard.path().join("home");
        std::fs::create_dir_all(&home)?;
        Ok(Self {
            _tmpdir_guard: tmpdir_guard,
            tmpdir,
            _home_guard: home_guard,
            home,
        })
    }

    fn cmd(&self) -> anyhow::Result<assert_cmd::Command> {
        let mut cmd = assert_cmd::Command::new(codex_utils_cargo_bin::cargo_bin("codex")?);
        cmd.env("CODEX_HOME", &self.home)
            .env("TMPDIR", &self.tmpdir)
            .env("TEMP", &self.tmpdir)
            .env("TMP", &self.tmpdir)
            .env("RUST_LOG", "off");
        Ok(cmd)
    }

    fn assert_no_arg0_leak(&self, stderr: &str) {
        assert_arg0_setup_exercised(&self.home, stderr);
        let arg0 = leaked_arg0_dirs(&self.home);
        assert!(
            arg0.is_empty(),
            "CODEX_HOME/tmp/arg0 leaked session dirs: {arg0:?}"
        );
    }
}

/// `codex exec --help` exits 0 without leaking the multitool arg0 guard.
#[test]
fn cli_exec_help_leaves_no_tmpdirs() -> anyhow::Result<()> {
    let test = IsolatedCli::new()?;

    let assert = test.cmd()?.arg("exec").arg("--help").assert().code(0);
    let output = assert.get_output();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    pretty_assertions::assert_eq!(stderr.as_str(), "");
    assert!(
        stdout.contains("Usage"),
        "expected clap help in stdout, got: {stdout}"
    );

    test.assert_no_arg0_leak(&stderr);
    Ok(())
}

/// `codex exec` with an invalid flag exits 2 without leaking the guard.
#[test]
fn cli_exec_invalid_args_leave_no_tmpdirs() -> anyhow::Result<()> {
    let test = IsolatedCli::new()?;

    let assert = test
        .cmd()?
        .arg("exec")
        .arg("--definitely-invalid-option")
        .assert()
        .code(2);
    let output = assert.get_output();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    pretty_assertions::assert_eq!(stdout.as_str(), "");
    assert!(
        stderr.contains("--definitely-invalid-option"),
        "expected clap error to mention the flag, got: {stderr}"
    );

    test.assert_no_arg0_leak(&stderr);
    Ok(())
}

/// Ordinary multitool errors must not panic when stderr fails.
///
/// `codex --remote ws://... exec` returns an ordinary anyhow error through the
/// shared termination path. Same `/dev/full` contract as the `codex-exec`
/// twin: exit 1, no panic, no arg0 leak. Skips where `/dev/full` is absent.
#[test]
fn cli_exec_stderr_failure_preserves_exit_code() -> anyhow::Result<()> {
    let dev_full = Path::new("/dev/full");
    if !dev_full.exists() {
        eprintln!(
            "SKIP: /dev/full does not exist on this platform; failing-stderr path not exercised"
        );
        return Ok(());
    }
    let test = IsolatedCli::new()?;
    let stderr_file = std::fs::File::options().write(true).open(dev_full)?;
    let bin = codex_utils_cargo_bin::cargo_bin("codex")?;
    let mut cmd = std::process::Command::new(bin);
    cmd.env("CODEX_HOME", &test.home)
        .env("TMPDIR", &test.tmpdir)
        .env("TEMP", &test.tmpdir)
        .env("TMP", &test.tmpdir)
        .env("RUST_LOG", "off")
        .arg("--remote")
        .arg("ws://127.0.0.1:1")
        .arg("exec")
        .arg("--skip-git-repo-check")
        .arg("hello")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::from(stderr_file));
    let output = cmd.output()?;
    pretty_assertions::assert_eq!(
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
    let arg0 = leaked_arg0_dirs(&test.home);
    assert!(
        arg0.is_empty(),
        "CODEX_HOME/tmp/arg0 leaked session dirs: {arg0:?}"
    );
    Ok(())
}
