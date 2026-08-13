use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorProfile {
    Dev,
    Prod,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Stage {
    Parse,
    Semantic,
    Cfg,
    Resolve,
}

impl Stage {
    fn as_str(&self) -> &'static str {
        match self {
            Stage::Parse => "parse",
            Stage::Semantic => "semantic",
            Stage::Cfg => "cfg",
            Stage::Resolve => "resolve",
        }
    }
}

#[derive(Error, Debug)]
pub enum EngineError {
    #[error("Not a valid ZIP file: {detail}")]
    InvalidZipHeader { detail: String },

    #[error("ZIP archive is corrupted or not a valid ZIP: {reason}")]
    ZipCorrupted { reason: String },

    #[error("Failed to read '{path}' from local filesystem: {detail}")]
    Io { path: String, detail: String },

    #[error("No parseable files (.js/.jsx/.ts/.tsx/.mjs/.cjs/.mts/.cts) found in the archive")]
    NoParseableFiles,

    #[error("A file caused a Rust panic during processing (dev profile is fail-closed): {file}: {message}")]
    AbortedOnPanic { file: String, message: String },
}

impl From<EngineError> for String {
    fn from(err: EngineError) -> Self {
        err.to_string()
    }
}

#[derive(Error, Debug, Serialize)]
pub enum FileError {
    #[error("'{path}' is not valid UTF-8")]
    InvalidUtf8 { path: String },

    #[error("Parser panicked on '{path}' (unrecoverable syntax errors)")]
    ParsePanicked { path: String },

    #[error("Semantic analysis failed for '{path}': {detail}")]
    SemanticBuildFailed { path: String, detail: String },

    #[error("CFG unavailable for '{path}': {detail}")]
    CfgUnavailable { path: String, detail: String },

    #[error("Resolution failed for specifier '{specifier}' in '{path}': {detail}")]
    ResolutionFailed {
        path: String,
        specifier: String,
        detail: String,
    },

    #[error("Rust panic while processing '{path}' at stage {stage:?}: {message}")]
    RustPanic {
        path: String,
        stage: Stage,
        message: String,
    },
}

impl FileError {
    pub fn file_path(&self) -> &str {
        match self {
            FileError::InvalidUtf8 { path }
            | FileError::ParsePanicked { path }
            | FileError::SemanticBuildFailed { path, .. }
            | FileError::CfgUnavailable { path, .. }
            | FileError::ResolutionFailed { path, .. }
            | FileError::RustPanic { path, .. } => path,
        }
    }

    pub fn render_message(&self, profile: ErrorProfile) -> String {
        match profile {
            ErrorProfile::Dev => self.to_string(),
            ErrorProfile::Prod => match self {
                FileError::InvalidUtf8 { .. } => "file_encoding_error".to_string(),
                FileError::ParsePanicked { .. } => "parse_failed".to_string(),
                FileError::SemanticBuildFailed { .. } => "semantic_analysis_failed".to_string(),
                FileError::CfgUnavailable { .. } => "cfg_unavailable".to_string(),
                FileError::ResolutionFailed { .. } => "import_resolution_failed".to_string(),
                FileError::RustPanic { stage, .. } => {
                    format!("internal_error_at_{}", stage.as_str())
                }
            },
        }
    }
}

pub fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload (unknown cause)".to_string()
    }
}

#[derive(Error, Debug)]
pub enum PhysicsError {
    #[error("Cannot initialize physics: graph has 0 nodes")]
    EmptyGraph,

    #[error("Position/velocity buffer length mismatch: expected {expected}, got {actual}")]
    BufferSizeMismatch { expected: usize, actual: usize },

    #[error("Node index {index} out of bounds (graph has {node_count} nodes)")]
    NodeIndexOutOfBounds { index: usize, node_count: usize },

    #[error("Physics simulation panicked during tick {tick_number}: {message}")]
    TickPanic { tick_number: u64, message: String },
}
