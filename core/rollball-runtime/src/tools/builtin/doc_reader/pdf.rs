//! PDF text extraction via `lopdf`.
//!
//! Walks the PDF object tree, decodes text streams, and concatenates
//! all text content in page order.

use std::path::Path;

use super::ExtractOptions;

/// Maximum pages to process (safety cap).
const MAX_PAGES: usize = 200;

/// Extract text content from a PDF file.
pub fn extract_text(path: &Path, opts: &ExtractOptions) -> Result<String, String> {
    let doc = lopdf::Document::load(path).map_err(|e| format!("Failed to open PDF: {e}"))?;

    let total_pages = doc.get_pages().len();
    if total_pages == 0 {
        return Ok("(empty PDF)".to_string());
    }

    let start = opts.start_page.unwrap_or(1).max(1).min(total_pages);
    let end = opts.end_page.unwrap_or(total_pages).max(start).min(total_pages);
    let max_pages = end.saturating_sub(start).saturating_add(1).min(MAX_PAGES);

    let mut output = String::new();

    for (i, page_id) in doc
        .get_pages()
        .iter()
        .enumerate()
        .skip(start.saturating_sub(1))
        .take(max_pages)
    {
        let page_num = i + 1;
        let text = match doc.extract_text(&[*page_id.0]) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let text = text.trim();
        if text.is_empty() {
            continue;
        }

        output.push_str(&format!("\n[Page {page_num}]\n"));
        output.push_str(text);
        output.push('\n');
    }

    if output.is_empty() {
        return Ok("(no extractable text in PDF)".to_string());
    }

    let summary = if end - start + 1 < total_pages {
        format!("\n[Extracted pages {start}-{end} of {total_pages}]\n")
    } else {
        format!("\n[{total_pages} pages total]\n")
    };
    output.push_str(&summary);

    Ok(output)
}
