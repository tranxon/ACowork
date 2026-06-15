//! Memory allocation and export function helpers for WASM tools
//!
//! Provides the `tool_entry!` macro that generates the `#[no_mangle]`
//! WASM export functions needed by the AgentCowork Runtime.

use std::cell::RefCell;

thread_local! {
    /// Output buffer for returning results to the host.
    static OUTPUT_BUFFER: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Write output to the thread-local buffer and return pointer.
///
/// # Safety
/// This is designed to be called from WASM `execute` export.
/// The host reads from WASM linear memory after `execute` returns.
pub fn set_output(json: &str) -> u32 {
    OUTPUT_BUFFER.with(|buf| {
        *buf.borrow_mut() = json.as_bytes().to_vec();
        buf.borrow().as_ptr() as u32
    })
}

/// Get the length of the output buffer.
pub fn output_len() -> u32 {
    OUTPUT_BUFFER.with(|buf| buf.borrow().len() as u32)
}

/// Macro that generates the WASM entry point for a tool.
///
/// This creates the `execute` and `output_len` export functions
/// that the AgentCowork Runtime calls.
///
/// # Example
///
/// ```ignore
/// use acowork_tool_sdk::{ToolInput, ToolOutput, ToolError, tool_entry};
///
/// fn my_tool(input: ToolInput) -> Result<ToolOutput, ToolError> {
///     let name = input.get("name")?;
///     Ok(ToolOutput::from(json!({"greeting": format!("Hello, {}!", name)})))
/// }
///
/// tool_entry!(my_tool);
/// ```
#[macro_export]
macro_rules! tool_entry {
    ($func:path) => {
        /// WASM entry point: execute(input_ptr, input_len) -> output_ptr
        ///
        /// The host calls this with a pointer to the JSON input
        /// in WASM linear memory. The tool reads the input,
        /// processes it, and writes the result to a global buffer.
        /// Returns the pointer to the output buffer.
        #[unsafe(no_mangle)]
        pub extern "C" fn execute(input_ptr: u32, input_len: u32) -> u32 {
            // Read input from WASM linear memory
            let input_bytes =
                unsafe { core::slice::from_raw_parts(input_ptr as *const u8, input_len as usize) };

            // Parse input
            let input = match $crate::ToolInput::from_bytes(input_bytes) {
                Ok(input) => input,
                Err(e) => {
                    let error_json = format!(r#"{{"error": "Invalid input: {}"}}"#, e);
                    return $crate::exports::set_output(&error_json);
                }
            };

            // Execute tool function
            match $func(input) {
                Ok(output) => {
                    let json = output.to_json_string();
                    $crate::exports::set_output(&json)
                }
                Err(e) => {
                    let error_json = format!(r#"{{"error": "{}"}}"#, e);
                    $crate::exports::set_output(&error_json)
                }
            }
        }

        /// WASM entry point: output_len() -> u32
        ///
        /// Returns the length of the output buffer after execute.
        #[unsafe(no_mangle)]
        pub extern "C" fn output_len() -> u32 {
            $crate::exports::output_len()
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::{ToolError, ToolInput, ToolOutput};

    #[test]
    fn test_set_output_and_read() {
        let ptr = set_output(r#"{"result": 42}"#);
        assert!(ptr > 0);
        assert_eq!(output_len(), 14); // length of {"result": 42}
    }

    #[test]
    fn test_output_len_empty() {
        // After setting and then clearing
        set_output("");
        assert_eq!(output_len(), 0);
    }

    #[test]
    fn test_tool_entry_integration() {
        fn greet(input: ToolInput) -> Result<ToolOutput, ToolError> {
            let name = input.get("name")?;
            Ok(ToolOutput::from(serde_json::json!({
                "greeting": format!("Hello, {}!", name)
            })))
        }

        let input = ToolInput::from_json(r#"{"name": "World"}"#).unwrap();
        let result = greet(input);
        assert!(result.is_ok());
        let output = result.unwrap();
        assert_eq!(output.data["greeting"], "Hello, World!");
    }
}
