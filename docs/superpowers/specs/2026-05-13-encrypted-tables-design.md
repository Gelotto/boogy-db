# Per-Table Encryption

## Problem

Some tables contain sensitive data (tokens, PII, credentials) that should be encrypted at rest. Encryption should be opt-in per table, transparent to the caller after setup, and impose no overhead on unencrypted tables.

## Design

### API

```rust
// Create an encrypted table with a 256-bit key
let key: [u8; 32] = /* caller-provided */;
db.create_table_encrypted("secrets", &[
    ColumnDef::new("token", Type::Text),
    ColumnDef::new("data", Type::Blob),
], &key)?;

// All operations work identically — encryption is transparent
db.insert("secrets", &[("token", Value::Text("abc".into()))])?;
let row = db.get("secrets", 1)?;

// On reopen, provide keys for encrypted tables
let db = BoogyDb::open("path")?;
db.unlock_table("secrets", &key)?;
// Operations on "secrets" now work. Without unlock, they return an error.
```

### Key Management

- Caller provides raw `[u8; 32]` (AES-256 key). Key derivation (Argon2, HKDF, HSM, etc.) is the caller's responsibility.
- The key is stored in `TableMeta` in memory only — **never written to disk**.
- The system page records which tables are encrypted (a flag in the table registry) but not the keys.
- On reopen, the caller must call `unlock_table(name, &key)` for each encrypted table before operating on it. Operations on a locked encrypted table return `BoogyError::TableLocked`.
- Different tables can use different keys, or the same key — caller's choice.

### Encryption Algorithm

AES-256-GCM via the `aes-gcm` crate (pure Rust with AES-NI hardware acceleration).

- **Authenticated encryption** — GCM provides both confidentiality and integrity (replaces CRC32 checksum with a cryptographic MAC).
- **Per-page nonce** — 12 bytes: `[page_no:4][random:8]`. The random component ensures unique nonces even if a page is rewritten. Generated fresh on every page write.

### Encrypted Page Layout

```
[nonce:12][encrypted_data:4068][auth_tag:16]
```

Total: 12 + 4068 + 16 = 4096 bytes (same page size).

The `encrypted_data` is the AES-256-GCM ciphertext of the normal 4068-byte page payload. After decryption, it becomes a standard Page with header, offset array, and row data. The 4068-byte payload is slightly smaller than the normal 4092 usable bytes (4096 - 4 for CRC32), meaning encrypted tables have slightly lower per-page capacity (~0.6% less).

Unencrypted pages are unchanged — they keep the existing `[page_data:4092][crc32:4]` layout.

### Encrypt/Decrypt Points

The in-memory page cache always holds **plaintext** pages. Encryption is transparent at disk I/O boundaries only:

**Encrypt (plaintext → ciphertext):**
- `sync_all()` — when flushing cached pages to the data file
- WAL writes in `commit_write()` — when appending after-images to WAL

**Decrypt (ciphertext → plaintext):**
- `read_page_from_disk()` — when loading a page from the data file on cache miss
- WAL replay during `open()` — when reading WAL entries for crash recovery

The encrypt/decrypt decision is per-page: look up the table that owns the page, check if it's encrypted, get the key from TableMeta.

### Page-to-Table Mapping

To know which key to use when reading/writing a page, we need to map page numbers to tables. Options:

**Approach:** Store a `page_table_map: HashMap<u32, u32>` (page_no → table_id) in PageFile, populated during open from the B+ tree root pages in the system page registry. When a new page is allocated for a table, register it in the map. The map is only needed for encrypted tables — unencrypted pages bypass the lookup entirely.

Actually simpler: **pass the encryption context through the call chain.** BTreeWriter/BTreeReader already know which table they're operating on. Add an optional `&Cipher` parameter to page read/write operations:

```rust
// PageFile gets encryption-aware methods
pub fn read_page_encrypted(&self, page_no: u32, cipher: Option<&Cipher>) -> Result<Arc<Page>>
pub fn write_page_to_disk_encrypted(&self, page_no: u32, data: &[u8; PAGE_SIZE], cipher: Option<&Cipher>) -> Result<()>
```

Where `Cipher` wraps the AES-256-GCM key and provides encrypt/decrypt:

```rust
pub struct Cipher {
    key: aes_gcm::Aes256Gcm,
}

impl Cipher {
    pub fn new(key: &[u8; 32]) -> Self { ... }
    pub fn encrypt_page(&self, page_no: u32, plaintext: &[u8; PAGE_SIZE]) -> [u8; PAGE_SIZE] { ... }
    pub fn decrypt_page(&self, page_no: u32, ciphertext: &[u8; PAGE_SIZE]) -> Result<[u8; PAGE_SIZE]> { ... }
}
```

### Changes to TableMeta

```rust
pub struct TableMeta {
    // ... existing fields ...
    pub encrypted: bool,         // persisted in system page
    pub cipher: Option<Cipher>,  // in-memory only, set by unlock_table
}
```

The system page format adds one byte per table: `[encrypted: u8]` (0 or 1).

### Changes to BoogyDb

**create_table_encrypted(name, columns, key):**
1. Same as `create_table` but sets `meta.encrypted = true` and `meta.cipher = Some(Cipher::new(key))`
2. System page records `encrypted = true` for this table

**unlock_table(name, key):**
1. Look up table in registry
2. Verify `meta.encrypted == true`
3. Set `meta.cipher = Some(Cipher::new(key))`
4. Verify the key is correct by reading and decrypting the table's root page. If decryption fails (GCM auth tag mismatch), return `BoogyError::InvalidKey` and clear the cipher.

**All read/write operations:**
- Check `meta.cipher` before page I/O
- If `Some(cipher)`: encrypt before disk write, decrypt after disk read
- If `None` and `meta.encrypted`: return `BoogyError::TableLocked`
- If `None` and `!meta.encrypted`: normal unencrypted path (no overhead)

### Error Types

Add to `BoogyError`:
- `TableLocked(String)` — operation on encrypted table without key
- `InvalidKey(String)` — wrong key provided to unlock_table
- `DecryptionFailed(String)` — page decryption failed (corruption or wrong key)

### Index Encryption

Secondary indexes for encrypted tables are also encrypted with the same key. The IndexTree pages go through the same encrypt/decrypt path since they use the same PageFile.

### WAL Encryption

WAL entries for encrypted tables store encrypted page data:
- `commit_write` encrypts after-images before writing to WAL
- WAL replay during `open` decrypts entries before applying to the page cache

This means the WAL file contains ciphertext for encrypted tables, plaintext for unencrypted tables. Each WAL entry already has a `table_id` field, which is used to look up the cipher.

**Boot problem:** During crash recovery on open, we need to decrypt WAL entries, but the table registry (which tells us which tables are encrypted) is stored in the system page (page 0), which may itself be in the WAL. Solution: the system page is NEVER encrypted. Only data/index pages are encrypted. The system page stores table names, columns, and the `encrypted` flag in plaintext. This is acceptable because the system page contains only schema metadata, not user data.

### Performance Impact

AES-256-GCM with AES-NI: ~1 cycle/byte. For a 4KB page:
- Encrypt: ~4K cycles ≈ 1.5µs
- Decrypt: ~4K cycles ≈ 1.5µs

Impact on encrypted tables:
- Point get: +1.5µs on cache miss (plaintext after first load). Zero overhead on cache hit.
- Insert: +1.5µs for WAL write (encrypt the after-image).
- No impact on unencrypted tables (zero overhead — the cipher check is `Option::is_some()`).

### Dependency

```toml
[dependencies]
aes-gcm = "0.10"
rand = "0.8"  # for nonce generation
```

Note: `rand` is already in dev-dependencies. Move to dependencies or use `getrandom` directly.

## Files Changed

- `src/crypto.rs` — New: Cipher struct, encrypt_page, decrypt_page
- `src/table.rs` — Add `encrypted: bool`, `cipher: Option<Cipher>` to TableMeta
- `src/error.rs` — Add TableLocked, InvalidKey, DecryptionFailed
- `src/db.rs` — Add create_table_encrypted, unlock_table. Thread cipher through page I/O.
- `src/file.rs` — Add encryption-aware read/write methods (or pass cipher through existing ones)
- `src/wal.rs` — Encrypt/decrypt WAL entries based on table_id → cipher mapping
- `Cargo.toml` — Add aes-gcm dependency

## Scope

This spec covers per-table AES-256-GCM encryption at the page level. Not in scope:
- Key derivation (caller's responsibility)
- Key rotation (would require re-encrypting all pages — future spec)
- Column-level encryption (encrypting individual values instead of whole pages)
- Encrypted system page (schema metadata stays plaintext)
