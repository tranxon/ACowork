//! ToolInput/ToolOutput/ToolError — core types for WASM tool SDK
//!
//! These types mirror the host-side `ToolInput`/`ToolOutput` in
//! `rollball-runtime/src/tools/wasm/wit.rs` but are designed for
//! the WASM guest side (no wasmtime dependency).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Error type for tool execution.
#[derive(Debug)]
pub enum ToolError {
    /// A required parameter was missing
    MissingParam(String),
    /// A parameter had the wrong type
    TypeError(String),
    /// JSON serialization/deserialization error
    JsonError(String),
    /// General tool error
    Other(String),
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToolError::MissingParam(key) => write!(f, "Missing required parameter: {}", key),
            ToolError::TypeError(msg) => write!(f, "Type error: {}", msg),
            ToolError::JsonError(msg) => write!(f, "JSON error: {}", msg),
            ToolError::Other(msg) => write!(f, "Error: {}", msg),
        }
    }
}

impl From<serde_json::Error> for ToolError {
    fn from(e: serde_json::Error) -> Self {
        ToolError::JsonError(e.to_string())
    }
}

/// Typed input for a WASM tool.
///
/// Parsed from the JSON arguments provided by the LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolInput {
    /// The input parameters as a JSON object
    pub params: Value,
}

impl ToolInput {
    /// Create a ToolInput from a JSON value.
    pub fn new(params: Value) -> Self {
        Self { params }
    }

    /// Parse ToolInput from raw JSON bytes (from WASM memory).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ToolError> {
        let params: Value = serde_json::from_slice(bytes)?;
        Ok(Self { params })
    }

    /// Parse ToolInput from a JSON string.
    pub fn from_json(json: &str) -> Result<Self, ToolError> {
        let params: Value = serde_json::from_str(json)?;
        Ok(Self { params })
    }

    /// Get a string parameter, returning error if missing or wrong type.
    pub fn get(&self, key: &str) -> Result<String, ToolError> {
        match self.params.get(key) {
            Some(Value::String(s)) => Ok(s.clone()),
            Some(v) => Ok(v.to_string()),
            None => Err(ToolError::MissingParam(key.to_string())),
        }
    }

    /// Get a string parameter, returning None if missing.
    pub fn get_optional(&self, key: &str) -> Option<String> {
        self.params.get(key).and_then(|v| v.as_str()).map(|s| s.to_string())
    }

    /// Get a numeric parameter.
    pub fn get_number(&self, key: &str) -> Result<f64, ToolError> {
        match self.params.get(key).and_then(|v| v.as_f64()) {
            Some(n) => Ok(n),
            None => Err(ToolError::MissingParam(key.to_string())),
        }
    }

    /// Get a boolean parameter.
    pub fn get_bool(&self, key: &str) -> Result<bool, ToolError> {
        match self.params.get(key).and_then(|v| v.as_bool()) {
            Some(b) => Ok(b),
            None => Err(ToolError::MissingParam(key.to_string())),
        }
    }

    /// Get a raw JSON value parameter.
    pub fn get_value(&self, key: &str) -> Result<&Value, ToolError> {
        self.params.get(key)
            .ok_or_else(|| ToolError::MissingParam(key.to_string()))
    }
}

/// Typed output from a WASM tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolOutput {
    /// The output data as a JSON object
    pub data: Value,
}

impl ToolOutput {
    /// Create a ToolOutput from a JSON value.
    pub fn from(data: Value) -> Self {
        Self { data }
    }

    /// Create an empty successful output.
    pub fn ok() -> Self {
        Self {
            data: serde_json::json!({"status": "ok"}),
        }
    }

    /// Create an output with a message.
    pub fn with_message(msg: &str) -> Self {
        Self {
            data: serde_json::json!({"status": "ok", "message": msg}),
        }
    }

    /// Serialize to JSON bytes for WASM memory transfer.
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(&self.data).unwrap_or_default()
    }

    /// Serialize to JSON string.
    pub fn to_json_string(&self) -> String {
        serde_json::to_string(&self.data).unwrap_or_default()
    }
}

// The WASM entry point macro is `tool_entry!` in exports.rs.
// It generates the `execute` and `output_len` export functions.
// Example usage:
//
// ```ignore
// use rollball_tool_sdk::{ToolInput, ToolOutput, ToolError, tool_entry};
//
// fn my_tool(input: ToolInput) -> Result<ToolOutput, ToolError> {
//     let name = input.get("name")?;
//     Ok(ToolOutput::from(json!({"greeting": format!("Hello, {}!", name)})))
// }
//
// tool_entry!(my_tool);
// ```

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_input_from_json() {
        let input = ToolInput::from_json(r#"{"name": "test", "count": 42}"#).unwrap();
        assert_eq!(input.get("name").unwrap(), "test");
        assert_eq!(input.get_number("count").unwrap(), 42.0);
    }

    #[test]
    fn test_tool_input_missing_param() {
        let input = ToolInput::from_json(r#"{"name": "test"}"#).unwrap();
        assert!(input.get("missing").is_err());
        assert!(matches!(input.get("missing"), Err(ToolError::MissingParam(_))));
    }

    #[test]
    fn test_tool_input_get_optional() {
        let input = ToolInput::from_json(r#"{"name": "test"}"#).unwrap();
        assert_eq!(input.get_optional("name"), Some("test".to_string()));
        assert!(input.get_optional("missing").is_none());
    }

    #[test]
    fn test_tool_input_get_bool() {
        let input = ToolInput::from_json(r#"{"active": true}"#).unwrap();
        assert!(input.get_bool("active").unwrap());
    }

    #[test]
    fn test_tool_input_get_value() {
        let input = ToolInput::from_json(r#"{"nested": {"a": 1}}"#).unwrap();
        let val = input.get_value("nested").unwrap();
        assert_eq!(val["a"], 1);
    }

    #[test]
    fn test_tool_output_from() {
        let output = ToolOutput::from(serde_json::json!({"result": 42}));
        let bytes = output.to_bytes();
        assert!(!bytes.is_empty());
    }

    #[test]
    fn test_tool_output_ok() {
        let output = ToolOutput::ok();
        assert_eq!(output.data["status"], "ok");
    }

    #[test]
    fn test_tool_output_with_message() {
        let output = ToolOutput::with_message("done");
        assert_eq!(output.data["status"], "ok");
        assert_eq!(output.data["message"], "done");
    }

    #[test]
    fn test_tool_error_display() {
        let err = ToolError::MissingParam("key".to_string());
        assert!(err.to_string().contains("key"));

        let err = ToolError::TypeError("expected string".to_string());
        assert!(err.to_string().contains("Type error"));
    }
}
