//! Silent process exits that preserve exact stderr bytes.
//!
//! When fallible startup code has already printed its user-facing diagnostic
//! (or intentionally prints nothing), it returns [`SilentExit`] so the outer
//! `main` can map it to an exit code after the arg0 temporary-directory guard
//! has been dropped, without emitting Rust's `Error: <Debug>` termination line.

/// Marker error for exits whose diagnostics were already emitted.
///
/// The process exit code is carried in the error so it survives `?`
/// propagation through [`crate::arg0_dispatch_or_else`] to the outer `main`,
/// which converts it to [`std::process::ExitCode`] after all guards are
/// dropped.
#[derive(Debug)]
pub struct SilentExit {
    code: u8,
}

impl SilentExit {
    /// Silent exit with an explicit code (for example clap's 0 or 2).
    pub fn new(code: u8) -> Self {
        Self { code }
    }

    /// Silent success (exit 0, for example `--help`).
    pub fn success() -> Self {
        Self { code: 0 }
    }

    /// Silent failure (exit 1, the pre-existing exec error code).
    pub fn failure() -> Self {
        Self { code: 1 }
    }

    /// The process exit code to return.
    pub fn code(&self) -> u8 {
        self.code
    }
}

impl std::fmt::Display for SilentExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let code = self.code;
        write!(f, "silent exit with code {code}")
    }
}

impl std::error::Error for SilentExit {}

/// Map the result of [`crate::arg0_dispatch_or_else`] to a process exit code.
///
/// This preserves the previous `fn main() -> anyhow::Result<()>` stderr bytes:
/// silent exits print nothing extra (their diagnostic was already emitted, or
/// none was needed), while all other errors print `Error: {Debug}`, matching
/// Rust's `Termination` for `anyhow::Result`.
pub fn exit_code_from_result(result: anyhow::Result<()>) -> std::process::ExitCode {
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            if let Some(silent) = err.downcast_ref::<SilentExit>() {
                std::process::ExitCode::from(silent.code())
            } else {
                eprintln!("Error: {err:?}");
                std::process::ExitCode::FAILURE
            }
        }
    }
}
