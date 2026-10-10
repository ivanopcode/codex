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

/// Map the result of [`crate::arg0_dispatch_or_else`] to a termination value.
///
/// This preserves the previous `fn main() -> anyhow::Result<()>` behavior
/// exactly: silent exits become `Ok` with their code (no extra output; their
/// diagnostic was already emitted or none was needed), while all other errors
/// are returned as `Err` so the standard `Termination` implementation prints
/// `Error: {Debug}` and returns `FAILURE`. Delegating (rather than calling
/// `eprintln!`) preserves the base behavior of ignoring terminal stderr write
/// failures instead of panicking.
///
/// All temporary-directory guards are dropped inside
/// [`crate::arg0_dispatch_or_else`] before this mapping runs, so returning
/// `Err` here still cleans up before the runtime prints the diagnostic.
pub fn exit_result_from_result(
    result: anyhow::Result<()>,
) -> Result<std::process::ExitCode, anyhow::Error> {
    match result {
        Ok(()) => Ok(std::process::ExitCode::SUCCESS),
        Err(err) => {
            if let Some(code) = err.downcast_ref::<SilentExit>().map(SilentExit::code) {
                Ok(std::process::ExitCode::from(code))
            } else {
                eprintln!("Error: {err:?}");
                Ok(std::process::ExitCode::FAILURE)
            }
        }
    }
}
