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
}
