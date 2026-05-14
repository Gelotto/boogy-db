use crate::page::{Page, PAGE_HEADER_SIZE, PAGE_SIZE};

/// Maximum payload bytes per overflow page (page minus header minus checksum).
pub const OVERFLOW_PAYLOAD_MAX: usize = PAGE_SIZE - PAGE_HEADER_SIZE - 4;

/// Sentinel byte at the end of an inline row to signal overflow.
pub const OVERFLOW_MARKER: u8 = 0xFF;

/// Size of the overflow trailer appended to inline row data:
/// [marker:1][first_page:4][remaining_len:4] = 9 bytes.
pub const OVERFLOW_TRAILER_SIZE: usize = 9;

/// Returns `true` if `row_bytes` ends with an overflow trailer.
///
/// Checks that the row is at least `OVERFLOW_TRAILER_SIZE` bytes and that the
/// byte at `len - OVERFLOW_TRAILER_SIZE` is the `OVERFLOW_MARKER`.
pub fn has_overflow(row_bytes: &[u8]) -> bool {
    if row_bytes.len() < OVERFLOW_TRAILER_SIZE {
        return false;
    }
    row_bytes[row_bytes.len() - OVERFLOW_TRAILER_SIZE] == OVERFLOW_MARKER
}

/// Decode the overflow trailer from the last 9 bytes of `row_bytes`.
///
/// Returns `(inline_len, first_page, remaining_len)` where:
/// - `inline_len` is the number of row bytes before the trailer
/// - `first_page` is the page number of the first overflow page
/// - `remaining_len` is the total number of bytes stored in overflow pages
///
/// # Panics
///
/// Panics if `row_bytes.len() < OVERFLOW_TRAILER_SIZE`.
pub fn decode_overflow_trailer(row_bytes: &[u8]) -> (usize, u32, u32) {
    assert!(row_bytes.len() >= OVERFLOW_TRAILER_SIZE);
    let trailer_start = row_bytes.len() - OVERFLOW_TRAILER_SIZE;
    // skip the marker byte
    let first_page = u32::from_le_bytes(
        row_bytes[trailer_start + 1..trailer_start + 5]
            .try_into()
            .unwrap(),
    );
    let remaining_len = u32::from_le_bytes(
        row_bytes[trailer_start + 5..trailer_start + 9]
            .try_into()
            .unwrap(),
    );
    (trailer_start, first_page, remaining_len)
}

/// Append an overflow trailer to `buf`.
///
/// Writes `[OVERFLOW_MARKER, first_page (LE), remaining_len (LE)]`.
pub fn append_overflow_trailer(buf: &mut Vec<u8>, first_page: u32, remaining_len: u32) {
    buf.push(OVERFLOW_MARKER);
    buf.extend_from_slice(&first_page.to_le_bytes());
    buf.extend_from_slice(&remaining_len.to_le_bytes());
}

/// Build an overflow page from a data slice.
///
/// Copies up to `OVERFLOW_PAYLOAD_MAX` bytes from `data` into the page body,
/// sets the payload length and next-page pointer, and updates the checksum.
///
/// # Panics
///
/// Panics if `data.len() > OVERFLOW_PAYLOAD_MAX`.
pub fn build_overflow_page(data: &[u8], next_page: u32) -> Page {
    assert!(data.len() <= OVERFLOW_PAYLOAD_MAX);
    let mut page = Page::new_overflow();
    let start = PAGE_HEADER_SIZE;
    page.data[start..start + data.len()].copy_from_slice(data);
    page.set_overflow_payload_len(data.len() as u16);
    page.set_overflow_next(next_page);
    page.update_checksum();
    page
}

/// Return the payload slice stored in an overflow page.
pub fn read_overflow_payload(page: &Page) -> &[u8] {
    let len = page.overflow_payload_len() as usize;
    &page.data[PAGE_HEADER_SIZE..PAGE_HEADER_SIZE + len]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_overflow_positive() {
        let mut buf = vec![0u8; 20];
        // Place the marker at the correct position
        append_overflow_trailer(&mut buf, 42, 1000);
        assert!(has_overflow(&buf));
    }

    #[test]
    fn has_overflow_negative() {
        let buf = vec![0u8; 20];
        assert!(!has_overflow(&buf));
    }

    #[test]
    fn has_overflow_too_short() {
        let buf = vec![OVERFLOW_MARKER; 5]; // shorter than OVERFLOW_TRAILER_SIZE
        assert!(!has_overflow(&buf));
    }

    #[test]
    fn trailer_roundtrip() {
        let mut buf = vec![1, 2, 3, 4, 5]; // inline data
        append_overflow_trailer(&mut buf, 99, 8192);
        assert!(has_overflow(&buf));

        let (inline_len, first_page, remaining) = decode_overflow_trailer(&buf);
        assert_eq!(inline_len, 5);
        assert_eq!(first_page, 99);
        assert_eq!(remaining, 8192);
    }

    #[test]
    fn page_build_and_read() {
        let payload = b"hello overflow world!";
        let page = build_overflow_page(payload, 7);

        assert!(page.is_overflow());
        assert!(page.verify_checksum());
        assert_eq!(page.overflow_payload_len() as usize, payload.len());
        assert_eq!(page.overflow_next(), 7);

        let recovered = read_overflow_payload(&page);
        assert_eq!(recovered, payload);
    }
}
