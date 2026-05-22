//! PDF text extraction via `pdf-extract`.
//!
//! Uses font-rendered text extraction (via `pdf` + `fontdue`), which
//! handles Type1, CFF, TrueType, and CID-keyed fonts correctly —
//! including academic papers with mathematical symbol fonts that
//! confuse simpler decoders like `lopdf`.

use std::path::Path;

use super::ExtractOptions;

/// Maximum pages to process (safety cap).
const MAX_PAGES: usize = 200;

/// Extract text content from a PDF file.
pub fn extract_text(path: &Path, opts: &ExtractOptions) -> Result<String, String> {
    // pdf_extract returns one entry per page; empty pages are empty strings.
    let pages: Vec<String> = pdf_extract::extract_text_by_pages(path)
        .map_err(|e| format!("Failed to extract PDF text: {e}"))?;

    let total_pages = pages.len();
    if total_pages == 0 {
        return Ok("(empty PDF)".to_string());
    }

    let start = opts.start_page.unwrap_or(1).max(1).min(total_pages);
    let end = opts.end_page.unwrap_or(total_pages).max(start).min(total_pages);
    let max_pages = end.saturating_sub(start).saturating_add(1).min(MAX_PAGES);

    let mut output = String::new();
    let mut count = 0;

    for page_num in start..=end {
        if count >= max_pages {
            break;
        }
        let idx = page_num.saturating_sub(1);
        let text = match pages.get(idx) {
            Some(t) => t.trim(),
            None => continue,
        };
        if text.is_empty() {
            continue;
        }

        output.push_str(&format!("\n[Page {page_num}]\n"));
        output.push_str(text);
        output.push('\n');
        count += 1;
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
