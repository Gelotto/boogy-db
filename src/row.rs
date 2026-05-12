use crate::error::{BoogyError, Result};
use crate::value::Value;

// Type tags
const TAG_NULL: u8 = 0;
const TAG_TEXT: u8 = 1;
const TAG_INTEGER: u8 = 2;
const TAG_REAL: u8 = 3;
const TAG_BLOB: u8 = 4;
const TAG_BOOLEAN: u8 = 5;

/// Encode a row (_id + columns) into compact binary format.
///
/// Layout: [id_len:2][id_bytes][num_cols:2][col_id:2][tag:1][value]...
pub fn encode_row(id: &str, columns: &[(u16, &Value)]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64);
    let id_bytes = id.as_bytes();
    buf.extend_from_slice(&(id_bytes.len() as u16).to_le_bytes());
    buf.extend_from_slice(id_bytes);
    buf.extend_from_slice(&(columns.len() as u16).to_le_bytes());
    for &(col_id, val) in columns {
        buf.extend_from_slice(&col_id.to_le_bytes());
        encode_value(&mut buf, val);
    }
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

/// Decoded row: the _id and all column values.
pub struct DecodedRow {
    pub id: String,
    pub columns: Vec<(u16, Value)>,
}

/// Decode a full row from bytes.
pub fn decode_row(data: &[u8]) -> Result<DecodedRow> {
    let mut offset = 0;

    // _id
    ensure_bytes(data, offset, 2)?;
    let id_len = u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap()) as usize;
    offset += 2;
    ensure_bytes(data, offset, id_len)?;
    let id = String::from_utf8(data[offset..offset + id_len].to_vec())
        .map_err(|_| BoogyError::Corruption("invalid utf8 in _id".into()))?;
    offset += id_len;

    // columns
    ensure_bytes(data, offset, 2)?;
    let num_cols = u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap()) as usize;
    offset += 2;

    let mut columns = Vec::with_capacity(num_cols);
    for _ in 0..num_cols {
        ensure_bytes(data, offset, 2)?;
        let col_id = u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap());
        offset += 2;
        let (val, consumed) = decode_value(&data[offset..])?;
        offset += consumed;
        columns.push((col_id, val));
    }

    Ok(DecodedRow { id, columns })
}

/// Extract just the _id from row bytes without decoding columns.
pub fn extract_id(data: &[u8]) -> Result<&str> {
    ensure_bytes(data, 0, 2)?;
    let id_len = u16::from_le_bytes(data[0..2].try_into().unwrap()) as usize;
    ensure_bytes(data, 2, id_len)?;
    std::str::from_utf8(&data[2..2 + id_len])
        .map_err(|_| BoogyError::Corruption("invalid utf8 in _id".into()))
}

/// Extract a single column value by column ID without decoding all columns.
pub fn extract_column(data: &[u8], target_col_id: u16) -> Result<Option<Value>> {
    let mut offset = 0;

    // Skip _id
    ensure_bytes(data, offset, 2)?;
    let id_len = u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap()) as usize;
    offset += 2 + id_len;

    // num columns
    ensure_bytes(data, offset, 2)?;
    let num_cols = u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap()) as usize;
    offset += 2;

    for _ in 0..num_cols {
        ensure_bytes(data, offset, 2)?;
        let col_id = u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap());
        offset += 2;
        if col_id == target_col_id {
            let (val, _) = decode_value(&data[offset..])?;
            return Ok(Some(val));
        }
        // Skip this value
        offset += value_byte_size(&data[offset..])?;
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

fn value_byte_size(data: &[u8]) -> Result<usize> {
    ensure_bytes(data, 0, 1)?;
    match data[0] {
        TAG_NULL => Ok(1),
        TAG_TEXT | TAG_BLOB => {
            ensure_bytes(data, 1, 4)?;
            let len = u32::from_le_bytes(data[1..5].try_into().unwrap()) as usize;
            Ok(5 + len)
        }
        TAG_INTEGER | TAG_REAL => Ok(9),
        TAG_BOOLEAN => Ok(2),
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
        let encoded = encode_row("row_1", &cols);
        let decoded = decode_row(&encoded).unwrap();
        assert_eq!(decoded.id, "row_1");
        assert_eq!(decoded.columns.len(), 5);
        assert_eq!(decoded.columns[0], (0, Value::Text("alice".into())));
        assert_eq!(decoded.columns[1], (1, Value::Integer(42)));
        assert_eq!(decoded.columns[3], (3, Value::Boolean(true)));
        assert_eq!(decoded.columns[4], (4, Value::Null));
    }

    #[test]
    fn test_extract_id() {
        let encoded = encode_row("my_uuid", &[(0, &Value::Integer(1))]);
        assert_eq!(extract_id(&encoded).unwrap(), "my_uuid");
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
        let encoded = encode_row("id1", &cols);

        assert_eq!(extract_column(&encoded, 1).unwrap(), Some(Value::Integer(42)));
        assert_eq!(extract_column(&encoded, 2).unwrap(), Some(Value::Boolean(false)));
        assert_eq!(extract_column(&encoded, 99).unwrap(), None);
    }

    #[test]
    fn test_blob_round_trip() {
        let blob_data = vec![0xFF, 0x00, 0xAB, 0xCD];
        let v0 = Value::Blob(blob_data.clone());
        let cols = vec![(0u16, &v0)];
        let encoded = encode_row("blob_row", &cols);
        let decoded = decode_row(&encoded).unwrap();
        assert_eq!(decoded.columns[0].1, Value::Blob(blob_data));
    }

    #[test]
    fn test_empty_string() {
        let v0 = Value::Text(String::new());
        let cols = vec![(0u16, &v0)];
        let encoded = encode_row("empty", &cols);
        let decoded = decode_row(&encoded).unwrap();
        assert_eq!(decoded.columns[0].1, Value::Text(String::new()));
    }
}
