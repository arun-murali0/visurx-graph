//! Format adapter for the wasm↔JS boundary.
//!
//! Two independent encode paths right now, deliberately NOT unified yet:
//!
//! - `encode_json` — works today on anything that's already
//!   `serde::Serialize` (e.g. `scan::ScanResult`). This is the local-testing
//!   path: readable output, no protobuf decoding needed on the JS side,
//!   useful right now while ts-proto codegen isn't wired up yet.
//! - `encode_proto` — works today on anything that's already
//!   `prost::Message` (e.g. `proto::RepoGraph`). This is the production
//!   path: compact bytes, what the real wasm exports should return.
//!
//! These two are kept separate on purpose for now: none of the
//! prost-generated types currently derive `serde::Serialize` (that needs a
//! `build.rs` change — adding `.type_attribute(".", "#[derive(serde::Serialize)]")` —
//! which is the "write schema" step, done deliberately last, once the
//! schema itself is settled). Once that lands, `encode_dual` below becomes
//! usable for graph data too, letting the *same* `RepoGraph` value be
//! requested as either format from one function. Until then, testing JSON
//! output for graph data means testing against a plain Rust struct (like
//! `ScanResult`) that already has both, or a value that already implements
//! both bounds today — not a hypothetical restriction, just not wired for
//! `RepoGraph` specifically yet.
//!
//! `save_json`/`save_proto`/`save_dual` below write the encoded bytes to
//! disk (creating parent directories as needed) IN ADDITION to returning
//! them — this is what `test_pipeline` and any other native test binary
//! should call instead of hand-rolling `fs::write` at each call site. This
//! only makes sense off the wasm target: browser WASM has no real
//! filesystem, and `std::fs` calls there compile fine but always fail at
//! runtime with an `Unsupported` I/O error — that's a correct, harmless
//! `Err(AdapterError::Io(..))` if it's ever accidentally called from
//! wasm-exposed code, not a crash, but it's not meant to be called there.
//! Every `#[wasm_bindgen]`-exported function should stick to `encode_json`/
//! `encode_proto`/`encode_dual` and hand bytes back to JS, never `save_*`.

use serde::Serialize;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// Real, compact protobuf bytes — the production default.
    Proto,
    /// UTF-8 JSON bytes — local development/debugging only. Same data,
    /// same field names, just human-readable.
    Json,
}

impl OutputFormat {
    /// Parses a format flag as it arrives from JS (`"proto"` / `"json"`,
    /// case-insensitive). Anything unrecognized defaults to `Proto` —
    /// an unrecognized flag should behave like production, not silently
    /// drop into a debug mode.
    pub fn from_flag(flag: &str) -> Self {
        if flag.eq_ignore_ascii_case("json") {
            OutputFormat::Json
        } else {
            OutputFormat::Proto
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    #[error("failed to encode as protobuf: {0}")]
    Proto(String),
    #[error("failed to encode as JSON: {0}")]
    Json(String),
    #[error("failed to write '{path}': {detail}")]
    Io { path: String, detail: String },
}

/// Encodes `value` as JSON bytes. Works today for anything already
/// `Serialize` — e.g. `scan::ScanResult`.
pub fn encode_json<T: Serialize>(value: &T) -> Result<Vec<u8>, AdapterError> {
    serde_json::to_vec(value).map_err(|e| AdapterError::Json(e.to_string()))
}

/// Encodes `value` as real protobuf bytes. Works today for anything already
/// `prost::Message` — e.g. `proto::RepoGraph`. Replaces the ad hoc
/// `.expect("encoding RepoGraph should never fail")` that used to live
/// directly in `proto::encode_repo_graph` with a real `Result`.
pub fn encode_proto<T: prost::Message>(value: &T) -> Result<Vec<u8>, AdapterError> {
    let mut buf = Vec::new();
    value
        .encode(&mut buf)
        .map_err(|e| AdapterError::Proto(e.to_string()))?;
    Ok(buf)
}

/// Encodes `value` as either format, per `format`. Only callable for a `T`
/// that is BOTH `prost::Message` and `Serialize` at once — today that's
/// nothing in `proto::generated` (see module doc comment), but any future
/// type satisfying both bounds already works here without changes to this
/// function.
pub fn encode_dual<T>(value: &T, format: OutputFormat) -> Result<Vec<u8>, AdapterError>
where
    T: prost::Message + Serialize,
{
    match format {
        OutputFormat::Proto => encode_proto(value),
        OutputFormat::Json => encode_json(value),
    }
}

fn write_to_disk(bytes: &[u8], path: &Path) -> Result<(), AdapterError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| AdapterError::Io {
            path: parent.display().to_string(),
            detail: e.to_string(),
        })?;
    }
    std::fs::write(path, bytes).map_err(|e| AdapterError::Io {
        path: path.display().to_string(),
        detail: e.to_string(),
    })
}

/// Encodes `value` as JSON and writes it to `path` (parent directories
/// created as needed), returning the same bytes that were written.
pub fn save_json<T: Serialize>(value: &T, path: impl AsRef<Path>) -> Result<Vec<u8>, AdapterError> {
    let bytes = encode_json(value)?;
    write_to_disk(&bytes, path.as_ref())?;
    Ok(bytes)
}

/// Encodes `value` as protobuf and writes it to `path` (parent directories
/// created as needed), returning the same bytes that were written.
pub fn save_proto<T: prost::Message>(
    value: &T,
    path: impl AsRef<Path>,
) -> Result<Vec<u8>, AdapterError> {
    let bytes = encode_proto(value)?;
    write_to_disk(&bytes, path.as_ref())?;
    Ok(bytes)
}

/// Encodes `value` as either format (per `format`) and writes it to `path`.
/// Same `T: prost::Message + Serialize` bound as `encode_dual` — see its
/// doc comment for why that's not yet satisfiable for real graph types.
pub fn save_dual<T>(
    value: &T,
    format: OutputFormat,
    path: impl AsRef<Path>,
) -> Result<Vec<u8>, AdapterError>
where
    T: prost::Message + Serialize,
{
    match format {
        OutputFormat::Proto => save_proto(value, path),
        OutputFormat::Json => save_json(value, path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serialize;

    #[derive(Serialize, prost::Message, PartialEq, Clone)]
    struct DualTestMessage {
        #[prost(string, tag = "1")]
        name: String,
        #[prost(uint32, tag = "2")]
        count: u32,
    }

    #[test]
    fn json_output_is_readable_utf8() {
        let value = DualTestMessage {
            name: "hello".to_string(),
            count: 3,
        };
        let bytes = encode_json(&value).unwrap();
        let text = String::from_utf8(bytes).expect("JSON output must be valid UTF-8");
        assert!(text.contains("\"name\":\"hello\""));
        assert!(text.contains("\"count\":3"));
    }

    #[test]
    fn proto_output_round_trips() {
        let value = DualTestMessage {
            name: "hello".to_string(),
            count: 3,
        };
        let bytes = encode_proto(&value).unwrap();
        let decoded = DualTestMessage::decode(bytes.as_slice()).unwrap();
        assert_eq!(decoded, value);
    }

    #[test]
    fn dual_dispatch_picks_the_right_encoder() {
        let value = DualTestMessage {
            name: "x".to_string(),
            count: 1,
        };

        let json_bytes = encode_dual(&value, OutputFormat::Json).unwrap();
        assert!(String::from_utf8(json_bytes).unwrap().starts_with('{'));

        let proto_bytes = encode_dual(&value, OutputFormat::Proto).unwrap();
        assert_eq!(
            DualTestMessage::decode(proto_bytes.as_slice()).unwrap(),
            value
        );
    }

    #[test]
    fn from_flag_defaults_to_proto_for_unrecognized_input() {
        assert_eq!(OutputFormat::from_flag("json"), OutputFormat::Json);
        assert_eq!(OutputFormat::from_flag("JSON"), OutputFormat::Json);
        assert_eq!(OutputFormat::from_flag("proto"), OutputFormat::Proto);
        assert_eq!(OutputFormat::from_flag("nonsense"), OutputFormat::Proto);
    }

    #[test]
    fn save_json_writes_real_readable_file_creating_parent_dirs() {
        let dir = std::env::temp_dir().join(format!("adapter_test_{}", std::process::id()));
        let path = dir.join("nested").join("out.json"); // parent doesn't exist yet

        let value = DualTestMessage {
            name: "hello".to_string(),
            count: 3,
        };
        let returned_bytes = save_json(&value, &path).unwrap();

        let on_disk = std::fs::read(&path).unwrap();
        assert_eq!(on_disk, returned_bytes);
        assert!(String::from_utf8(on_disk)
            .unwrap()
            .contains("\"name\":\"hello\""));

        std::fs::remove_dir_all(&dir).ok(); // best-effort cleanup
    }

    #[test]
    fn save_proto_writes_bytes_that_round_trip_from_disk() {
        let dir = std::env::temp_dir().join(format!("adapter_test_proto_{}", std::process::id()));
        let path = dir.join("out.pb");

        let value = DualTestMessage {
            name: "x".to_string(),
            count: 42,
        };
        save_proto(&value, &path).unwrap();

        let on_disk = std::fs::read(&path).unwrap();
        assert_eq!(DualTestMessage::decode(on_disk.as_slice()).unwrap(), value);

        std::fs::remove_dir_all(&dir).ok();
    }
}
