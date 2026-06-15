//! Example: image_filter tool built with acowork-tool-sdk
//!
//! Demonstrates how to build a WASM tool using the SDK.
//! This is NOT a real image filter — it just shows the pattern.
//!
//! Build with: cargo build --target wasm32-wasip2 --release

use acowork_tool_sdk::{ToolError, ToolInput, ToolOutput, tool_entry};

/// Image filter tool: applies a named filter to an image URL.
///
/// Input: { "image_url": string, "filter": string }
/// Output: { "filtered_image_url": string, "filter": string, "status": string }
fn image_filter(input: ToolInput) -> Result<ToolOutput, ToolError> {
    let image_url = input.get("image_url")?;
    let filter = input.get("filter")?;

    // In a real tool, this would call an image processing library.
    // For demonstration, we just return a mock result.
    let filtered_url = format!("{}_{}", filter, image_url);

    Ok(ToolOutput::from(serde_json::json!({
        "filtered_image_url": filtered_url,
        "filter": filter,
        "status": "success"
    })))
}

// Generate the WASM entry point
tool_entry!(image_filter);

fn main() {
    // When compiled for native, this runs a simple test.
    // When compiled for wasm32-wasip2, the tool_entry! macro
    // generates the `execute` export that the Runtime calls.
    let input = ToolInput::from_json(
        r#"{"image_url": "https://example.com/photo.jpg", "filter": "grayscale"}"#,
    )
    .unwrap();
    let output = image_filter(input).unwrap();
    println!("{}", output.to_json_string());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_image_filter_grayscale() {
        let input = ToolInput::from_json(
            r#"{"image_url": "https://example.com/photo.jpg", "filter": "grayscale"}"#,
        )
        .unwrap();

        let output = image_filter(input).unwrap();
        assert_eq!(output.data["status"], "success");
        assert_eq!(output.data["filter"], "grayscale");
        assert!(
            output.data["filtered_image_url"]
                .as_str()
                .unwrap()
                .contains("grayscale")
        );
    }

    #[test]
    fn test_image_filter_missing_url() {
        let input = ToolInput::from_json(r#"{"filter": "blur"}"#).unwrap();
        let result = image_filter(input);
        assert!(result.is_err());
    }

    #[test]
    fn test_image_filter_missing_filter() {
        let input =
            ToolInput::from_json(r#"{"image_url": "https://example.com/test.jpg"}"#).unwrap();
        let result = image_filter(input);
        assert!(result.is_err());
    }
}
