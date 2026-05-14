use crate::error::{BoogyError, Result};
use crate::value::Value;

// Type tags
const TAG_NULL: u8 = 0;
const TAG_TEXT: u8 = 1;
const TAG_INTEGER: u8 = 2;
const TAG_REAL: u8 = 3;
const TAG_BLOB: u8 = 4;
const TAG_BOOLEAN: u8 = 5;

/// Encode a row (rowid + columns) into compact binary format with offset directory.
///
/// Layout:
///   [rowid:8]
///   [num_cols:2]
///   [offset_directory: num_cols × 4 bytes]
///     for each column (sorted by col_id): [col_id:2][data_offset:2]
///   [column_data]
///     for each column: [type_tag:1][value_bytes]
///
/// The offset directory enables O(1) column access via binary search on col_id.
pub fn encode_row(rowid: u64, columns: &[(u16, &Value)]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64);
    buf.extend_from_slice(&rowid.to_le_bytes());
    buf.extend_from_slice(&(columns.len() as u16).to_le_bytes());

    // Sort columns by col_id for binary search
    let mut sorted: Vec<(u16, &Value)> = columns.to_vec();
    sorted.sort_by_key(|(id, _)| *id);

    // First pass: encode all column data to compute offsets
    let mut col_data = Vec::with_capacity(48);
    let mut offsets: Vec<(u16, u16)> = Vec::with_capacity(sorted.len());
    for &(col_id, val) in &sorted {
        let data_offset = col_data.len() as u16;
        offsets.push((col_id, data_offset));
        encode_value(&mut col_data, val);
    }

    // Write offset directory
    for &(col_id, data_offset) in &offsets {
        buf.extend_from_slice(&col_id.to_le_bytes());
        buf.extend_from_slice(&data_offset.to_le_bytes());
    }

    // Write column data
    buf.extend_from_slice(&col_data);
    buf
}

fn encode_value(buf: &mut Vec<u8>, val: &Value) {
    match val {
        Value::Null => buf.push(TAG_NULL),
        Value::Text(s) => {
            buf.push(TAG_TEXT);
            let bytes = s.as_bytes();
            buf.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            buf.extend_from_slice(bytes);
        }
        Value::Integer(i) => {
            buf.push(TAG_INTEGER);
            buf.extend_from_slice(&i.to_le_bytes());
        }
        Value::Real(f) => {
            buf.push(TAG_REAL);
            buf.extend_from_slice(&f.to_le_bytes());
        }
        Value::Blob(b) => {
            buf.push(TAG_BLOB);
            buf.extend_from_slice(&(b.len() as u32).to_le_bytes());
            buf.extend_from_slice(b);
        }
        Value::Boolean(b) => {
            buf.push(TAG_BOOLEAN);
            buf.push(if *b { 1 } else { 0 });
        }
    }
}

/// Encode a single Value into bytes (type_tag + value_bytes).
pub fn encode_value_to_vec(val: &Value) -> Vec<u8> {
    let mut buf = Vec::with_capacity(16);
    encode_value(&mut buf, val);
    buf
}

/// Patch a raw row by replacing a single column's value in-place.
/// Returns a new Vec<u8> with the column replaced. Avoids full decode/encode.
/// Much faster than decode_row → merge → encode_row for single-column updates.
pub fn patch_row(data: &[u8], target_col_id: u16, new_val: &Value) -> Result<Vec<u8>> {
    ensure_bytes(data, 0, 10)?; // rowid(8) + num_cols(2)
    let num_cols = u16::from_le_bytes(data[8..10].try_into().unwrap()) as usize;
    if num_cols == 0 {
        return Ok(data.to_vec());
    }

    let dir_start = 10; // after rowid(8) + num_cols(2)
    let col_data_start = dir_start + num_cols * 4;

    // Binary search for the target column in the offset directory
    let mut lo = 0usize;
    let mut hi = num_cols;
    let mut found_idx = None;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let entry = dir_start + mid * 4;
        let col_id = u16::from_le_bytes(data[entry..entry + 2].try_into().unwrap());
        match col_id.cmp(&target_col_id) {
            std::cmp::Ordering::Less => lo = mid + 1,
            std::cmp::Ordering::Greater => hi = mid,
            std::cmp::Ordering::Equal => { found_idx = Some(mid); break; }
        }
    }

    let col_idx = match found_idx {
        Some(i) => i,
        None => return Ok(data.to_vec()), // column not found, return unchanged
    };

    // Get the byte range of the old value
    let entry = dir_start + col_idx * 4;
    let old_data_offset = u16::from_le_bytes(data[entry + 2..entry + 4].try_into().unwrap()) as usize;
    let old_abs_start = col_data_start + old_data_offset;
    let old_abs_end = if col_idx + 1 < num_cols {
        let next_entry = dir_start + (col_idx + 1) * 4;
        col_data_start + u16::from_le_bytes(data[next_entry + 2..next_entry + 4].try_into().unwrap()) as usize
    } else {
        data.len()
    };
    let old_len = old_abs_end - old_abs_start;

    // Encode the new value
    let new_encoded = encode_value_to_vec(new_val);
    let new_len = new_encoded.len();
    let size_diff = new_len as isize - old_len as isize;

    if size_diff == 0 {
        // Same size: direct overwrite, no offset updates needed
        let mut result = data.to_vec();
        result[old_abs_start..old_abs_end].copy_from_slice(&new_encoded);
        return Ok(result);
    }

    // Different size: splice and update subsequent offsets
    let new_total_len = (data.len() as isize + size_diff) as usize;
    let mut result = Vec::with_capacity(new_total_len);

    // Copy everything before the old value
    result.extend_from_slice(&data[..old_abs_start]);
    // Insert new value
    result.extend_from_slice(&new_encoded);
    // Copy everything after the old value
    result.extend_from_slice(&data[old_abs_end..]);

    // Update offset directory entries for columns AFTER the changed one
    for i in (col_idx + 1)..num_cols {
        let e = dir_start + i * 4;
        let old_off = u16::from_le_bytes(result[e + 2..e + 4].try_into().unwrap()) as i32;
        let new_off = old_off + size_diff as i32;
        if new_off < 0 || new_off > u16::MAX as i32 {
            return Err(BoogyError::Corruption("row offset overflow during patch".into()));
        }
        result[e + 2..e + 4].copy_from_slice(&(new_off as u16).to_le_bytes());
    }

    Ok(result)
}

/// Patch a raw row by replacing multiple columns. Applies patches sequentially.
pub fn patch_row_multi(data: &[u8], patches: &[(u16, &Value)]) -> Result<Vec<u8>> {
    let mut result = data.to_vec();
    for &(col_id, val) in patches {
        result = patch_row(&result, col_id, val)?;
    }
    Ok(result)
}

/// Decoded row: the rowid and all column values.
pub struct DecodedRow {
    pub id: u64,
    pub columns: Vec<(u16, Value)>,
}

/// Decode a full row from bytes.
pub fn decode_row(data: &[u8]) -> Result<DecodedRow> {
    let mut offset = 0;

    // rowid
    ensure_bytes(data, offset, 8)?;
    let id = u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap());
    offset += 8;

    // num columns
    ensure_bytes(data, offset, 2)?;
    let num_cols = u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap()) as usize;
    offset += 2;

    // Read offset directory
    let dir_size = num_cols * 4;
    ensure_bytes(data, offset, dir_size)?;
    let dir_start = offset;
    offset += dir_size;

    // Column data starts here
    let col_data_start = offset;

    let mut columns = Vec::with_capacity(num_cols);
    for i in 0..num_cols {
        let entry = dir_start + i * 4;
        let col_id = u16::from_le_bytes(data[entry..entry + 2].try_into().unwrap());
        let data_offset = u16::from_le_bytes(data[entry + 2..entry + 4].try_into().unwrap()) as usize;
        let abs_offset = col_data_start + data_offset;
        let (val, _) = decode_value(&data[abs_offset..])?;
        columns.push((col_id, val));
    }

    Ok(DecodedRow { id, columns })
}

/// Extract just the rowid from row bytes without decoding columns.
pub fn extract_id(data: &[u8]) -> Result<u64> {
    ensure_bytes(data, 0, 8)?;
    Ok(u64::from_le_bytes(data[0..8].try_into().unwrap()))
}

/// Extract the raw bytes (type_tag + value_bytes) of a column without decoding.
/// Returns a slice into `data` — zero allocation.
pub fn extract_column_raw(data: &[u8], target_col_id: u16) -> Result<Option<&[u8]>> {
    let mut offset = 0;
    ensure_bytes(data, offset, 8)?;
    offset += 8; // skip rowid

    ensure_bytes(data, offset, 2)?;
    let num_cols = u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap()) as usize;
    offset += 2;

    if num_cols == 0 {
        return Ok(None);
    }

    let dir_start = offset;
    let col_data_start = dir_start + num_cols * 4;

    let mut lo = 0usize;
    let mut hi = num_cols;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let entry = dir_start + mid * 4;
        ensure_bytes(data, entry, 4)?;
        let col_id = u16::from_le_bytes(data[entry..entry + 2].try_into().unwrap());
        match col_id.cmp(&target_col_id) {
            std::cmp::Ordering::Less => lo = mid + 1,
            std::cmp::Ordering::Greater => hi = mid,
            std::cmp::Ordering::Equal => {
                let data_offset = u16::from_le_bytes(data[entry + 2..entry + 4].try_into().unwrap()) as usize;
                let abs_offset = col_data_start + data_offset;
                // Find the end of this value
                let next_offset = if mid + 1 < num_cols {
                    let next_entry = dir_start + (mid + 1) * 4;
                    col_data_start + u16::from_le_bytes(data[next_entry + 2..next_entry + 4].try_into().unwrap()) as usize
                } else {
                    data.len()
                };
                return Ok(Some(&data[abs_offset..next_offset]));
            }
        }
    }
    Ok(None)
}

/// Extract a single column value by column ID in O(1) via binary search on the offset directory.
pub fn extract_column(data: &[u8], target_col_id: u16) -> Result<Option<Value>> {
    let mut offset = 0;

    // Skip rowid (fixed 8 bytes)
    ensure_bytes(data, offset, 8)?;
    offset += 8;

    // num columns
    ensure_bytes(data, offset, 2)?;
    let num_cols = u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap()) as usize;
    offset += 2;

    if num_cols == 0 {
        return Ok(None);
    }

    let dir_start = offset;
    let col_data_start = dir_start + num_cols * 4;

    // Binary search the offset directory for target_col_id
    let mut lo = 0usize;
    let mut hi = num_cols;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let entry = dir_start + mid * 4;
        ensure_bytes(data, entry, 4)?;
        let col_id = u16::from_le_bytes(data[entry..entry + 2].try_into().unwrap());
        match col_id.cmp(&target_col_id) {
            std::cmp::Ordering::Less => lo = mid + 1,
            std::cmp::Ordering::Greater => hi = mid,
            std::cmp::Ordering::Equal => {
                let data_offset = u16::from_le_bytes(data[entry + 2..entry + 4].try_into().unwrap()) as usize;
                let abs_offset = col_data_start + data_offset;
                let (val, _) = decode_value(&data[abs_offset..])?;
                return Ok(Some(val));
            }
        }
    }
    Ok(None)
}

fn decode_value(data: &[u8]) -> Result<(Value, usize)> {
    ensure_bytes(data, 0, 1)?;
    match data[0] {
        TAG_NULL => Ok((Value::Null, 1)),
        TAG_TEXT => {
            ensure_bytes(data, 1, 4)?;
            let len = u32::from_le_bytes(data[1..5].try_into().unwrap()) as usize;
            ensure_bytes(data, 5, len)?;
            let s = String::from_utf8(data[5..5 + len].to_vec())
                .map_err(|_| BoogyError::Corruption("invalid utf8".into()))?;
            Ok((Value::Text(s), 5 + len))
        }
        TAG_INTEGER => {
            ensure_bytes(data, 1, 8)?;
            let i = i64::from_le_bytes(data[1..9].try_into().unwrap());
            Ok((Value::Integer(i), 9))
        }
        TAG_REAL => {
            ensure_bytes(data, 1, 8)?;
            let f = f64::from_le_bytes(data[1..9].try_into().unwrap());
            Ok((Value::Real(f), 9))
        }
        TAG_BLOB => {
            ensure_bytes(data, 1, 4)?;
            let len = u32::from_le_bytes(data[1..5].try_into().unwrap()) as usize;
            ensure_bytes(data, 5, len)?;
            Ok((Value::Blob(data[5..5 + len].to_vec()), 5 + len))
        }
        TAG_BOOLEAN => {
            ensure_bytes(data, 1, 1)?;
            Ok((Value::Boolean(data[1] != 0), 2))
        }
        tag => Err(BoogyError::Corruption(format!("unknown type tag: {tag}"))),
    }
}

fn ensure_bytes(data: &[u8], offset: usize, need: usize) -> Result<()> {
    if offset + need > data.len() {
        Err(BoogyError::Corruption(format!(
            "truncated: need {need} bytes at offset {offset}, have {}",
            data.len()
        )))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode_round_trip() {
        let v0 = Value::Text("alice".into());
        let v1 = Value::Integer(42);
        let v2 = Value::Real(3.14);
        let v3 = Value::Boolean(true);
        let v4 = Value::Null;
        let cols = vec![
            (0u16, &v0),
            (1, &v1),
            (2, &v2),
            (3, &v3),
            (4, &v4),
        ];
        let encoded = encode_row(1, &cols);
        let decoded = decode_row(&encoded).unwrap();
        assert_eq!(decoded.id, 1);
        assert_eq!(decoded.columns.len(), 5);
        assert_eq!(decoded.columns[0], (0, Value::Text("alice".into())));
        assert_eq!(decoded.columns[1], (1, Value::Integer(42)));
        assert_eq!(decoded.columns[3], (3, Value::Boolean(true)));
        assert_eq!(decoded.columns[4], (4, Value::Null));
    }

    #[test]
    fn test_extract_id() {
        let encoded = encode_row(42, &[(0, &Value::Integer(1))]);
        assert_eq!(extract_id(&encoded).unwrap(), 42);
    }

    #[test]
    fn test_extract_column() {
        let v0 = Value::Text("alice".into());
        let v1 = Value::Integer(42);
        let v2 = Value::Boolean(false);
        let cols = vec![
            (0u16, &v0),
            (1, &v1),
            (2, &v2),
        ];
        let encoded = encode_row(1, &cols);

        assert_eq!(extract_column(&encoded, 1).unwrap(), Some(Value::Integer(42)));
        assert_eq!(extract_column(&encoded, 2).unwrap(), Some(Value::Boolean(false)));
        assert_eq!(extract_column(&encoded, 99).unwrap(), None);
    }

    #[test]
    fn test_extract_column_binary_search() {
        // Test with many columns to verify binary search works
        let values: Vec<Value> = (0..20).map(|i| Value::Integer(i * 100)).collect();
        let cols: Vec<(u16, &Value)> = values.iter().enumerate().map(|(i, v)| (i as u16, v)).collect();
        let encoded = encode_row(99, &cols);

        // Access last column directly — should be O(1) via binary search
        assert_eq!(extract_column(&encoded, 19).unwrap(), Some(Value::Integer(1900)));
        assert_eq!(extract_column(&encoded, 0).unwrap(), Some(Value::Integer(0)));
        assert_eq!(extract_column(&encoded, 10).unwrap(), Some(Value::Integer(1000)));
        assert_eq!(extract_column(&encoded, 20).unwrap(), None);
    }

    #[test]
    fn test_blob_round_trip() {
        let blob_data = vec![0xFF, 0x00, 0xAB, 0xCD];
        let v0 = Value::Blob(blob_data.clone());
        let cols = vec![(0u16, &v0)];
        let encoded = encode_row(7, &cols);
        let decoded = decode_row(&encoded).unwrap();
        assert_eq!(decoded.columns[0].1, Value::Blob(blob_data));
    }

    #[test]
    fn test_empty_string() {
        let v0 = Value::Text(String::new());
        let cols = vec![(0u16, &v0)];
        let encoded = encode_row(8, &cols);
        let decoded = decode_row(&encoded).unwrap();
        assert_eq!(decoded.columns[0].1, Value::Text(String::new()));
    }

    #[test]
    fn test_unsorted_columns_get_sorted() {
        let v0 = Value::Integer(100);
        let v1 = Value::Integer(200);
        let v2 = Value::Integer(300);
        // Encode out of order
        let cols = vec![(2u16, &v2), (0, &v0), (1, &v1)];
        let encoded = encode_row(10, &cols);
        // Decode should return sorted by col_id
        let decoded = decode_row(&encoded).unwrap();
        assert_eq!(decoded.columns[0], (0, Value::Integer(100)));
        assert_eq!(decoded.columns[1], (1, Value::Integer(200)));
        assert_eq!(decoded.columns[2], (2, Value::Integer(300)));
    }

    // --- patch_row tests ---

    #[test]
    fn test_patch_row_same_size() {
        // Integer -> Integer is same size (9 bytes), triggers the direct overwrite path
        let v0 = Value::Integer(100);
        let v1 = Value::Text("hello".into());
        let cols = vec![(0u16, &v0), (1, &v1)];
        let encoded = encode_row(1, &cols);

        let patched = patch_row(&encoded, 0, &Value::Integer(999)).unwrap();
        let decoded = decode_row(&patched).unwrap();
        assert_eq!(decoded.id, 1);
        assert_eq!(decoded.columns[0], (0, Value::Integer(999)));
        assert_eq!(decoded.columns[1], (1, Value::Text("hello".into())));
    }

    #[test]
    fn test_patch_row_different_size() {
        // Text "hi" -> Text "goodbye" is different size, triggers splice path
        let v0 = Value::Text("hi".into());
        let v1 = Value::Integer(42);
        let cols = vec![(0u16, &v0), (1, &v1)];
        let encoded = encode_row(1, &cols);

        let patched = patch_row(&encoded, 0, &Value::Text("goodbye".into())).unwrap();
        let decoded = decode_row(&patched).unwrap();
        assert_eq!(decoded.id, 1);
        assert_eq!(decoded.columns[0], (0, Value::Text("goodbye".into())));
        assert_eq!(decoded.columns[1], (1, Value::Integer(42)));
    }

    #[test]
    fn test_patch_row_column_not_found() {
        let v0 = Value::Integer(1);
        let cols = vec![(0u16, &v0)];
        let encoded = encode_row(1, &cols);

        // Patching a column that doesn't exist returns unchanged data
        let patched = patch_row(&encoded, 99, &Value::Integer(999)).unwrap();
        assert_eq!(patched, encoded);
    }

    #[test]
    fn test_patch_row_zero_columns() {
        let cols: Vec<(u16, &Value)> = vec![];
        let encoded = encode_row(1, &cols);

        // Patching an empty row returns unchanged data
        let patched = patch_row(&encoded, 0, &Value::Integer(1)).unwrap();
        assert_eq!(patched, encoded);
    }

    #[test]
    fn test_patch_row_last_column() {
        // Patch the last column -- boundary case for offset calculation
        let v0 = Value::Integer(1);
        let v1 = Value::Integer(2);
        let v2 = Value::Text("short".into());
        let cols = vec![(0u16, &v0), (1, &v1), (2, &v2)];
        let encoded = encode_row(1, &cols);

        let patched = patch_row(&encoded, 2, &Value::Text("a much longer replacement string".into())).unwrap();
        let decoded = decode_row(&patched).unwrap();
        assert_eq!(decoded.columns[0], (0, Value::Integer(1)));
        assert_eq!(decoded.columns[1], (1, Value::Integer(2)));
        assert_eq!(decoded.columns[2], (2, Value::Text("a much longer replacement string".into())));
    }

    #[test]
    fn test_patch_row_first_column_shrinks() {
        // First column shrinks -- tests offset update for subsequent columns
        let v0 = Value::Text("long text value here".into());
        let v1 = Value::Integer(42);
        let cols = vec![(0u16, &v0), (1, &v1)];
        let encoded = encode_row(1, &cols);

        let patched = patch_row(&encoded, 0, &Value::Text("x".into())).unwrap();
        let decoded = decode_row(&patched).unwrap();
        assert_eq!(decoded.columns[0], (0, Value::Text("x".into())));
        assert_eq!(decoded.columns[1], (1, Value::Integer(42)));
    }

    // --- patch_row_multi tests ---

    #[test]
    fn test_patch_row_multi_multiple_columns() {
        let v0 = Value::Text("alice".into());
        let v1 = Value::Integer(30);
        let v2 = Value::Boolean(true);
        let cols = vec![(0u16, &v0), (1, &v1), (2, &v2)];
        let encoded = encode_row(1, &cols);

        let patched = patch_row_multi(
            &encoded,
            &[
                (0, &Value::Text("bob".into())),
                (1, &Value::Integer(25)),
            ],
        )
        .unwrap();
        let decoded = decode_row(&patched).unwrap();
        assert_eq!(decoded.columns[0], (0, Value::Text("bob".into())));
        assert_eq!(decoded.columns[1], (1, Value::Integer(25)));
        assert_eq!(decoded.columns[2], (2, Value::Boolean(true)));
    }

    #[test]
    fn test_patch_row_multi_empty_patches() {
        let v0 = Value::Integer(42);
        let cols = vec![(0u16, &v0)];
        let encoded = encode_row(1, &cols);

        let patched = patch_row_multi(&encoded, &[]).unwrap();
        assert_eq!(patched, encoded);
    }

    // --- encode_value_to_vec tests ---

    #[test]
    fn test_encode_value_to_vec_all_types() {
        // Null
        let buf = encode_value_to_vec(&Value::Null);
        assert_eq!(buf, vec![0]); // TAG_NULL

        // Integer
        let buf = encode_value_to_vec(&Value::Integer(42));
        assert_eq!(buf.len(), 9); // 1 tag + 8 bytes
        assert_eq!(buf[0], 2); // TAG_INTEGER

        // Real
        let buf = encode_value_to_vec(&Value::Real(3.14));
        assert_eq!(buf.len(), 9); // 1 tag + 8 bytes
        assert_eq!(buf[0], 3); // TAG_REAL

        // Boolean
        let buf = encode_value_to_vec(&Value::Boolean(true));
        assert_eq!(buf, vec![5, 1]); // TAG_BOOLEAN + 1
        let buf = encode_value_to_vec(&Value::Boolean(false));
        assert_eq!(buf, vec![5, 0]); // TAG_BOOLEAN + 0

        // Text
        let buf = encode_value_to_vec(&Value::Text("hi".into()));
        assert_eq!(buf[0], 1); // TAG_TEXT
        assert_eq!(buf.len(), 1 + 4 + 2); // tag + len(u32) + "hi"

        // Blob
        let buf = encode_value_to_vec(&Value::Blob(vec![0xAB, 0xCD]));
        assert_eq!(buf[0], 4); // TAG_BLOB
        assert_eq!(buf.len(), 1 + 4 + 2); // tag + len(u32) + blob
    }

    // --- extract_column_raw tests ---

    #[test]
    fn test_extract_column_raw_returns_raw_slice() {
        let v0 = Value::Integer(42);
        let v1 = Value::Text("hello".into());
        let cols = vec![(0u16, &v0), (1, &v1)];
        let encoded = encode_row(1, &cols);

        // extract_column_raw for column 0 should return the raw integer encoding
        let raw = extract_column_raw(&encoded, 0).unwrap().unwrap();
        assert_eq!(raw[0], 2); // TAG_INTEGER
        let i = i64::from_le_bytes(raw[1..9].try_into().unwrap());
        assert_eq!(i, 42);

        // Column not found
        let raw = extract_column_raw(&encoded, 99).unwrap();
        assert!(raw.is_none());
    }

    #[test]
    fn test_extract_column_raw_zero_columns() {
        let cols: Vec<(u16, &Value)> = vec![];
        let encoded = encode_row(1, &cols);
        let raw = extract_column_raw(&encoded, 0).unwrap();
        assert!(raw.is_none());
    }

    // --- truncated data tests ---

    #[test]
    fn test_decode_row_truncated_data() {
        // Just a few bytes -- not enough for even a rowid
        let short = vec![0u8; 5];
        assert!(decode_row(&short).is_err());
    }

    #[test]
    fn test_extract_id_truncated() {
        let short = vec![0u8; 4];
        assert!(extract_id(&short).is_err());
    }

    #[test]
    fn test_extract_column_truncated() {
        let short = vec![0u8; 5];
        assert!(extract_column(&short, 0).is_err());
    }
}
