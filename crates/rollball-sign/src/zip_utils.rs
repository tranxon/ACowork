//! ZIP file structure manipulation utilities
//!
//! Provides low-level ZIP structure parsing for inserting/extracting
//! the signing block between local file entries and the Central Directory,
//! following APK Signature Scheme v2 approach.

use crate::error::{Result, SignError};

/// EOCD (End of Central Directory) signature
const EOCD_SIGNATURE: [u8; 4] = [0x50, 0x4B, 0x05, 0x06];

/// Minimum EOCD record size (without comment)
const EOCD_MIN_SIZE: usize = 22;

/// Maximum EOCD comment size (per ZIP spec)
const EOCD_MAX_COMMENT_SIZE: usize = 65535;

/// Offset of the CD offset field within the EOCD record
const EOCD_CD_OFFSET_FIELD: usize = 16;

/// Find the End of Central Directory record offset in ZIP data
pub fn find_eocd(data: &[u8]) -> Result<usize> {
    // Search backward from the end for the EOCD signature
    let max_search = EOCD_MIN_SIZE + EOCD_MAX_COMMENT_SIZE;
    let search_start = data.len().saturating_sub(max_search);

    for offset in (search_start..=data.len().saturating_sub(EOCD_MIN_SIZE)).rev() {
        if offset + 4 <= data.len() && data[offset..offset + 4] == EOCD_SIGNATURE {
            return Ok(offset);
        }
    }

    Err(SignError::InvalidPackage("EOCD not found".into()))
}

/// Get the Central Directory offset from EOCD record
pub fn get_cd_offset(data: &[u8], eocd_offset: usize) -> Result<usize> {
    let field_start = eocd_offset + EOCD_CD_OFFSET_FIELD;
    if field_start + 4 > data.len() {
        return Err(SignError::InvalidPackage("EOCD too short to read CD offset".into()));
    }

    let cd_offset = u32::from_le_bytes([
        data[field_start],
        data[field_start + 1],
        data[field_start + 2],
        data[field_start + 3],
    ]) as usize;

    Ok(cd_offset)
}

/// Get the Central Directory size from EOCD record
fn get_cd_size(data: &[u8], eocd_offset: usize) -> Result<usize> {
    let field_start = eocd_offset + 12;
    if field_start + 4 > data.len() {
        return Err(SignError::InvalidPackage("EOCD too short to read CD size".into()));
    }

    let cd_size = u32::from_le_bytes([
        data[field_start],
        data[field_start + 1],
        data[field_start + 2],
        data[field_start + 3],
    ]) as usize;

    Ok(cd_size)
}

/// Insert a binary block between local file entries and the Central Directory
///
/// Adjusts CD local header offsets and EOCD CD offset to account for
/// the inserted block, keeping the ZIP file structurally valid.
pub fn insert_block_before_cd(zip_data: &[u8], block: &[u8]) -> Result<Vec<u8>> {
    let eocd_offset = find_eocd(zip_data)?;
    let cd_offset = get_cd_offset(zip_data, eocd_offset)?;
    let cd_size = get_cd_size(zip_data, eocd_offset)?;

    // Validate: CD should end where EOCD begins
    if cd_offset + cd_size != eocd_offset {
        return Err(SignError::InvalidPackage(
            "CD size does not match EOCD position".into(),
        ));
    }

    let delta = block.len();

    // Build output: [local entries] + [block] + [CD (unmodified)] + [adjusted EOCD]
    let mut result = Vec::with_capacity(zip_data.len() + delta);

    // Copy local file entries (before CD) — their positions are unchanged
    result.extend_from_slice(&zip_data[..cd_offset]);

    // Insert signing block
    result.extend_from_slice(block);

    // Copy Central Directory as-is (local header offsets are still valid)
    result.extend_from_slice(&zip_data[cd_offset..eocd_offset]);

    // Adjusted EOCD: update CD offset
    let mut eocd_data = zip_data[eocd_offset..].to_vec();
    adjust_eocd_cd_offset(&mut eocd_data, delta)?;
    result.extend_from_slice(&eocd_data);

    // Validate the new ZIP structure after insertion
    let new_cd_offset = cd_offset + block.len();
    let new_eocd_offset = result.len() - eocd_data.len();

    // Verify CD offset + CD size == EOCD offset
    if new_cd_offset + cd_size != new_eocd_offset {
        return Err(SignError::InvalidPackage(
            format!(
                "ZIP structure validation failed after block insertion: cd_offset({}) + cd_size({}) != eocd_offset({})",
                new_cd_offset, cd_size, new_eocd_offset
            ),
        ));
    }

    // Verify CD offset is within bounds
    if new_cd_offset >= result.len() {
        return Err(SignError::InvalidPackage(
            "CD offset out of bounds after block insertion".into(),
        ));
    }

    Ok(result)
}

/// Adjust CD offset in EOCD record by adding delta
fn adjust_eocd_cd_offset(eocd_data: &mut [u8], delta: usize) -> Result<()> {
    if eocd_data.len() < EOCD_CD_OFFSET_FIELD + 4 {
        return Err(SignError::InvalidPackage("EOCD too short to adjust".into()));
    }

    let cd_offset = u32::from_le_bytes([
        eocd_data[EOCD_CD_OFFSET_FIELD],
        eocd_data[EOCD_CD_OFFSET_FIELD + 1],
        eocd_data[EOCD_CD_OFFSET_FIELD + 2],
        eocd_data[EOCD_CD_OFFSET_FIELD + 3],
    ]);

    let new_cd_offset = cd_offset as usize + delta;
    let new_bytes = (new_cd_offset as u32).to_le_bytes();
    eocd_data[EOCD_CD_OFFSET_FIELD] = new_bytes[0];
    eocd_data[EOCD_CD_OFFSET_FIELD + 1] = new_bytes[1];
    eocd_data[EOCD_CD_OFFSET_FIELD + 2] = new_bytes[2];
    eocd_data[EOCD_CD_OFFSET_FIELD + 3] = new_bytes[3];

    Ok(())
}

/// Find and extract the binary signing block from raw file data
///
/// Searches for the signing block between local file entries and the
/// Central Directory by checking for the trailing magic before the CD.
///
/// Returns the raw signing block bytes (in the binary framing format)
/// if found, or None if no binary signing block is present.
pub fn find_binary_signing_block(data: &[u8], magic: &[u8; 16]) -> Result<Option<Vec<u8>>> {
    let eocd_offset = find_eocd(data)?;
    let cd_offset = get_cd_offset(data, eocd_offset)?;

    // Check for trailing magic before CD
    if cd_offset < 16 {
        return Ok(None);
    }

    if &data[cd_offset - 16..cd_offset] != magic {
        return Ok(None);
    }

    // Read size suffix (8 bytes before trailing magic)
    if cd_offset < 24 {
        return Ok(None);
    }
    let size_suffix = u64::from_le_bytes(
        data[cd_offset - 24..cd_offset - 16]
            .try_into()
            .map_err(|_| SignError::InvalidPackage("Failed to read size suffix".into()))?,
    ) as usize;

    // Calculate signing block boundaries
    // Format: [magic:16][size_prefix:8][content:size_suffix][size_suffix:8][magic:16]
    // Total = 16 + 8 + size_suffix + 8 + 16 = 48 + size_suffix
    let block_total = 48usize + size_suffix;
    if cd_offset < block_total {
        return Ok(None);
    }

    let block_start = cd_offset - block_total;
    let block_data = data[block_start..cd_offset].to_vec();

    // Verify leading magic
    if &block_data[0..16] != magic {
        return Ok(None);
    }

    // Verify size prefix matches size suffix
    let size_prefix = u64::from_le_bytes(
        block_data[16..24]
            .try_into()
            .map_err(|_| SignError::InvalidPackage("Failed to read size prefix".into()))?,
    ) as usize;

    if size_prefix != size_suffix {
        return Ok(None);
    }

    Ok(Some(block_data))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn create_test_zip_data() -> Vec<u8> {
        let buf = Cursor::new(Vec::new());
        let mut writer = zip::ZipWriter::new(buf);
        let options = zip::write::SimpleFileOptions::default();

        writer.start_file("hello.txt", options).unwrap();
        writer.write_all(b"Hello, World!").unwrap();

        writer.start_file("dir/nested.txt", options).unwrap();
        writer.write_all(b"Nested content").unwrap();

        writer.finish().unwrap().into_inner()
    }

    use std::io::Cursor;

    #[test]
    fn test_find_eocd() {
        let data = create_test_zip_data();
        let offset = find_eocd(&data).unwrap();
        assert!(offset < data.len());
        assert_eq!(&data[offset..offset + 4], &EOCD_SIGNATURE);
    }

    #[test]
    fn test_get_cd_offset() {
        let data = create_test_zip_data();
        let eocd_offset = find_eocd(&data).unwrap();
        let cd_offset = get_cd_offset(&data, eocd_offset).unwrap();
        assert!(cd_offset > 0);
        assert!(cd_offset < eocd_offset);
        // CD should start with the CD signature
        assert_eq!(&data[cd_offset..cd_offset + 4], &[0x50, 0x4B, 0x01, 0x02]);
    }

    #[test]
    fn test_insert_block_before_cd() {
        let data = create_test_zip_data();
        let block = b"TEST_BLOCK_DATA_HERE!!!";

        let result = insert_block_before_cd(&data, block).unwrap();

        // Result should be larger by the block size
        assert_eq!(result.len(), data.len() + block.len());

        // The block data should appear between local entries and CD
        let eocd_offset = find_eocd(&result).unwrap();
        let cd_offset = get_cd_offset(&result, eocd_offset).unwrap();

        // The block should be right before the CD
        let block_start = cd_offset - block.len();
        assert_eq!(&result[block_start..cd_offset], block);

        // The resulting ZIP should still be valid
        let reader = Cursor::new(&result);
        let archive = zip::ZipArchive::new(reader).unwrap();
        assert_eq!(archive.len(), 2);
    }

    #[test]
    fn test_find_binary_signing_block_absent() {
        let data = create_test_zip_data();
        let magic: [u8; 16] = *b"RBSign Block 42\0";
        let result = find_binary_signing_block(&data, &magic).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_find_binary_signing_block_present() {
        let data = create_test_zip_data();
        let magic: [u8; 16] = *b"RBSign Block 42\0";

        // Manually construct a signing block
        let content = b"test content";
        let content_size = content.len() as u64;
        let mut block = Vec::new();
        block.extend_from_slice(&magic);
        block.extend_from_slice(&content_size.to_le_bytes());
        block.extend_from_slice(content);
        block.extend_from_slice(&content_size.to_le_bytes());
        block.extend_from_slice(&magic);

        let signed_data = insert_block_before_cd(&data, &block).unwrap();
        let found = find_binary_signing_block(&signed_data, &magic)
            .unwrap()
            .unwrap();
        assert_eq!(found, block);
    }

    #[test]
    fn test_insert_block_validates_structure() {
        // Normal insertion should pass structural validation
        let data = create_test_zip_data();
        let block = b"VALIDATION_TEST_BLOCK";
        let result = insert_block_before_cd(&data, block).unwrap();

        // Verify the resulting ZIP is structurally valid
        let reader = Cursor::new(&result);
        let archive = zip::ZipArchive::new(reader).unwrap();
        assert_eq!(archive.len(), 2);

        // Verify CD offset consistency
        let eocd_offset = find_eocd(&result).unwrap();
        let cd_offset = get_cd_offset(&result, eocd_offset).unwrap();
        let cd_size = get_cd_size(&result, eocd_offset).unwrap();
        assert_eq!(cd_offset + cd_size, eocd_offset,
            "CD offset + CD size should equal EOCD offset after insertion");
    }
}
