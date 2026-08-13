use std::panic::{self, AssertUnwindSafe};

use crate::error::{panic_message, EngineError, ErrorProfile, FileError, Stage};

pub fn run_stage_safely<F, T>(path: &str, stage: Stage, f: F) -> Result<T, FileError>
where
    F: FnOnce() -> Result<T, FileError>,
{
    let result = panic::catch_unwind(AssertUnwindSafe(f));

    match result {
        Ok(inner_result) => inner_result,
        Err(payload) => Err(FileError::RustPanic {
            path: path.to_string(),
            stage,
            message: panic_message(payload.as_ref()),
        }),
    }
}

pub fn run_file_safely<F, T>(path: &str, f: F) -> Result<T, FileError>
where
    F: FnOnce() -> Result<T, FileError>,
{
    let result = panic::catch_unwind(AssertUnwindSafe(f));

    match result {
        Ok(inner_result) => inner_result,
        Err(payload) => Err(FileError::RustPanic {
            path: path.to_string(),
            stage: Stage::Parse,
            message: panic_message(payload.as_ref()),
        }),
    }
}

pub fn apply_batch_policy(
    file_error: &FileError,
    profile: ErrorProfile,
) -> Result<(), EngineError> {
    match (profile, file_error) {
        (ErrorProfile::Dev, FileError::RustPanic { path, message, .. }) => {
            Err(EngineError::AbortedOnPanic {
                file: path.clone(),
                message: message.clone(),
            })
        }
        _ => Ok(()),
    }
}

pub fn init_panic_hook() {
    #[cfg(target_arch = "wasm32")]
    {
        console_error_panic_hook::set_once();
    }
}
