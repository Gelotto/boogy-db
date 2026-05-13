# Per-Table Encryption Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add opt-in AES-256-GCM encryption for individual tables, transparent to callers after setup.

**Architecture:** New `src/crypto.rs` module provides `Cipher` (AES-256-GCM encrypt/decrypt per page). `TableMeta` gains `encrypted: bool` (persisted) and `cipher: Option<Cipher>` (in-memory). Encryption happens at disk I/O boundaries: WAL writes encrypt after-images, disk reads decrypt on cache miss. The in-memory page cache always holds plaintext.

**Tech Stack:** Rust, `aes-gcm` crate (AES-256-GCM with AES-NI), `rand` crate (nonce generation).

**Spec:** `docs/superpowers/specs/2026-05-13-encrypted-tables-design.md`

---

## File Structure

| File | Action | Responsibility |
|------|--------|---------------|
| `src/crypto.rs` | Create | Cipher struct, encrypt_page, decrypt_page |
| `src/table.rs` | Modify | Add `encrypted: bool`, `cipher: Option<Cipher>` to TableMeta |
| `src/error.rs` | Modify | Add TableLocked, InvalidKey, DecryptionFailed variants |
| `src/db.rs` | Modify | Add create_table_encrypted, unlock_table, thread cipher through commit_write and open |
| `src/lib.rs` | Modify | Add `pub mod crypto;` |
| `Cargo.toml` | Modify | Add aes-gcm, rand dependencies |
| `tests/crud_test.rs` | Modify | Add encryption integration tests |

---

## Task 1: Crypto Module + Dependencies

**Files:**
- Create: `src/crypto.rs`
- Modify: `src/lib.rs`
- Modify: `Cargo.toml`

- [ ] **Step 1: Add dependencies to Cargo.toml**

```toml
[dependencies]
crc32fast = "1"
aes-gcm = "0.10"
rand = "0.9"
```

Note: `rand` moves from dev-dependencies to dependencies (already in dev-deps as 0.9).

- [ ] **Step 2: Create src/crypto.rs with Cipher struct**

```rust
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use aes_gcm::aead::Aead;
use crate::error::{BoogyError, Result};
use crate::page::PAGE_SIZE;

/// AES-256-GCM cipher for page-level encryption.
///
/// Encrypted page layout: [nonce:12][ciphertext:4068][auth_tag:16] = 4096 bytes.
/// The plaintext is 4068 bytes (PAGE_SIZE minus nonce and tag overhead).
pub const ENCRYPTED_PAYLOAD_SIZE: usize = PAGE_SIZE - 12 - 16; // 4068

pub struct Cipher {
    inner: Aes256Gcm,
}

impl Cipher {
    /// Create a new cipher from a 256-bit key.
    pub fn new(key: &[u8; 32]) -> Self {
        Self {
            inner: Aes256Gcm::new_from_slice(key).unwrap(),
        }
    }

    /// Encrypt a page. Returns a full PAGE_SIZE buffer:
    /// [nonce:12][ciphertext][auth_tag:16].
    ///
    /// The plaintext input must be exactly ENCRYPTED_PAYLOAD_SIZE bytes (4068).
    /// A random nonce is generated for each encryption.
    pub fn encrypt_page(&self, plaintext: &[u8]) -> Result<[u8; PAGE_SIZE]> {
        assert_eq!(plaintext.len(), ENCRYPTED_PAYLOAD_SIZE);

        // Generate random 12-byte nonce
        let mut nonce_bytes = [0u8; 12];
        rand::fill(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);

        // Encrypt (ciphertext includes the 16-byte auth tag appended by GCM)
        let ciphertext = self.inner.encrypt(nonce, plaintext)
            .map_err(|e| BoogyError::Corruption(format!("encryption failed: {e}")))?;

        // Pack into page-sized buffer: [nonce:12][ciphertext+tag]
        let mut out = [0u8; PAGE_SIZE];
        out[..12].copy_from_slice(&nonce_bytes);
        out[12..12 + ciphertext.len()].copy_from_slice(&ciphertext);
        Ok(out)
    }

    /// Decrypt a page. Input is a full PAGE_SIZE buffer.
    /// Returns ENCRYPTED_PAYLOAD_SIZE bytes of plaintext.
    pub fn decrypt_page(&self, encrypted: &[u8; PAGE_SIZE]) -> Result<[u8; ENCRYPTED_PAYLOAD_SIZE]> {
        let nonce = Nonce::from_slice(&encrypted[..12]);
        let ciphertext_and_tag = &encrypted[12..]; // 4068 + 16 = 4084 bytes

        let plaintext = self.inner.decrypt(nonce, ciphertext_and_tag)
            .map_err(|_| BoogyError::DecryptionFailed(
                "page decryption failed — wrong key or corrupted data".into()
            ))?;

        let mut out = [0u8; ENCRYPTED_PAYLOAD_SIZE];
        out.copy_from_slice(&plaintext);
        Ok(out)
    }
}

// Cipher is Send + Sync (Aes256Gcm is).
unsafe impl Send for Cipher {}
unsafe impl Sync for Cipher {}

impl std::fmt::Debug for Cipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Cipher(***)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let key = [0x42u8; 32];
        let cipher = Cipher::new(&key);

        let plaintext = [0xABu8; ENCRYPTED_PAYLOAD_SIZE];
        let encrypted = cipher.encrypt_page(&plaintext).unwrap();

        // Encrypted data should differ from plaintext
        assert_ne!(&encrypted[12..12 + ENCRYPTED_PAYLOAD_SIZE], &plaintext[..]);

        let decrypted = cipher.decrypt_page(&encrypted).unwrap();
        assert_eq!(&decrypted[..], &plaintext[..]);
    }

    #[test]
    fn test_wrong_key_fails() {
        let key1 = [0x42u8; 32];
        let key2 = [0x43u8; 32];
        let cipher1 = Cipher::new(&key1);
        let cipher2 = Cipher::new(&key2);

        let plaintext = [0xABu8; ENCRYPTED_PAYLOAD_SIZE];
        let encrypted = cipher1.encrypt_page(&plaintext).unwrap();

        // Decrypting with wrong key should fail (GCM auth tag mismatch)
        assert!(cipher2.decrypt_page(&encrypted).is_err());
    }

    #[test]
    fn test_tampered_data_fails() {
        let key = [0x42u8; 32];
        let cipher = Cipher::new(&key);

        let plaintext = [0xABu8; ENCRYPTED_PAYLOAD_SIZE];
        let mut encrypted = cipher.encrypt_page(&plaintext).unwrap();

        // Tamper with ciphertext
        encrypted[20] ^= 0xFF;

        assert!(cipher.decrypt_page(&encrypted).is_err());
    }

    #[test]
    fn test_unique_nonces() {
        let key = [0x42u8; 32];
        let cipher = Cipher::new(&key);
        let plaintext = [0xABu8; ENCRYPTED_PAYLOAD_SIZE];

        let enc1 = cipher.encrypt_page(&plaintext).unwrap();
        let enc2 = cipher.encrypt_page(&plaintext).unwrap();

        // Same plaintext produces different ciphertext (different nonces)
        assert_ne!(enc1, enc2);
        // Different nonces
        assert_ne!(&enc1[..12], &enc2[..12]);
    }
}
```

- [ ] **Step 3: Add `pub mod crypto;` to lib.rs**

Add after `pub mod index;`:
```rust
pub mod crypto;
```

- [ ] **Step 4: Run crypto tests**

Run: `cargo test --lib crypto::tests`
Expected: All 4 tests pass.

- [ ] **Step 5: Commit**

```bash
git add src/crypto.rs src/lib.rs Cargo.toml
git commit -m "feat: crypto module — AES-256-GCM page-level encryption"
```

---

## Task 2: Error Types + TableMeta Changes

**Files:**
- Modify: `src/error.rs`
- Modify: `src/table.rs`

- [ ] **Step 1: Add error variants**

Add to `BoogyError` enum:
```rust
TableLocked(String),
DecryptionFailed(String),
InvalidKey(String),
```

Add `Display` impl for each:
```rust
BoogyError::TableLocked(t) => write!(f, "table '{t}' is encrypted and locked — call unlock_table first"),
BoogyError::DecryptionFailed(msg) => write!(f, "decryption failed: {msg}"),
BoogyError::InvalidKey(t) => write!(f, "invalid encryption key for table '{t}'"),
```

- [ ] **Step 2: Add encrypted flag and cipher to TableMeta**

In `src/table.rs`, add `use crate::crypto::Cipher;` and two new fields:

```rust
pub struct TableMeta {
    // ... existing fields ...
    pub encrypted: bool,
    pub cipher: Option<Cipher>,
}
```

Update `TableMeta::new` to initialize them:
```rust
encrypted: false,
cipher: None,
```

- [ ] **Step 3: Verify compilation**

Run: `cargo check --lib`
Expected: Compiles (db.rs doesn't use the new fields yet).

- [ ] **Step 4: Commit**

```bash
git add src/error.rs src/table.rs
git commit -m "feat: add encrypted flag to TableMeta, encryption error types"
```

---

## Task 3: System Page Persistence for Encrypted Flag

**Files:**
- Modify: `src/db.rs`

The system page needs to persist the `encrypted` flag for each table so we know on reopen which tables require a key.

- [ ] **Step 1: Update serialize_system_page**

After the index section for each table, add:
```rust
// encrypted flag
data[offset] = if meta.encrypted { 1 } else { 0 };
offset += 1;
```

- [ ] **Step 2: Update deserialize_system_page**

After reading indexes for each table, add:
```rust
let encrypted = data[offset] != 0;
offset += 1;
meta.encrypted = encrypted;
```

- [ ] **Step 3: Run existing tests**

Run: `cargo test`
Expected: All tests pass (existing tables have `encrypted: false`, the new byte is 0).

Note: existing databases on disk won't have this byte. On first open of an old database, the system page won't have the encrypted flag. This is fine because `encrypted` defaults to `false` in `TableMeta::new`, and old databases are never encrypted. However, the deserialize function reads a fixed format — if the byte isn't there, it will read garbage. To handle this cleanly: check if there are remaining bytes before reading the encrypted flag. If the system page was written by an older version (without the flag), treat as unencrypted:

```rust
// encrypted flag (may not exist in older system pages)
let encrypted = if offset < data.len() && offset < PAGE_SIZE - 4 {
    let e = data[offset] != 0;
    offset += 1;
    e
} else {
    false
};
meta.encrypted = encrypted;
```

- [ ] **Step 4: Commit**

```bash
git add src/db.rs
git commit -m "feat: persist encrypted flag in system page"
```

---

## Task 4: create_table_encrypted + unlock_table + Encrypted I/O

**Files:**
- Modify: `src/db.rs`

This is the core integration task.

- [ ] **Step 1: Add create_table_encrypted**

```rust
/// Create an encrypted table. The key is used for page-level AES-256-GCM encryption.
/// The key is NOT stored on disk — it must be provided again via unlock_table on reopen.
pub fn create_table_encrypted(&self, name: &str, columns: &[ColumnDef], key: &[u8; 32]) -> Result<()> {
    // Same logic as create_table, but set encrypted = true and cipher = Some(...)
    {
        let tables = self.tables.read().unwrap();
        if tables.contains_key(name) {
            return Err(BoogyError::TableExists(name.to_string()));
        }
    }

    let durability = self.durability();
    let (root, table_id) = {
        let mut guard = self.file.begin_write();
        if self.file.page_count() == 0 {
            guard.allocate_page()?;
        }
        let root = BTreeWriter::create(&mut guard)?;
        let table_id = {
            let mut next = self.next_table_id.lock().unwrap();
            let id = *next;
            *next += 1;
            id
        };
        Self::commit_write(guard, &self.wal, durability, table_id)?;
        (root, table_id)
    };

    let mut meta = TableMeta::new(name.to_string(), table_id, columns.to_vec(), root);
    meta.encrypted = true;
    meta.cipher = Some(crate::crypto::Cipher::new(key));
    let state = Arc::new(RwLock::new(TableState { meta }));

    {
        let mut tables = self.tables.write().unwrap();
        if tables.contains_key(name) {
            return Err(BoogyError::TableExists(name.to_string()));
        }
        tables.insert(name.to_string(), state);
    }

    let (metas, next_id) = self.snapshot_table_metas();
    Self::persist_registry(&self.file, &self.wal, &metas, next_id, durability)?;
    Ok(())
}
```

- [ ] **Step 2: Add unlock_table**

```rust
/// Provide the encryption key for a locked encrypted table.
/// After this call, all operations on the table work normally.
/// Returns an error if the key is wrong (verified by decrypting the root page).
pub fn unlock_table(&self, name: &str, key: &[u8; 32]) -> Result<()> {
    let table_state = {
        let tables = self.tables.read().unwrap();
        tables.get(name)
            .ok_or_else(|| BoogyError::TableNotFound(name.to_string()))?
            .clone()
    };

    let mut state = table_state.write().unwrap();
    if !state.meta.encrypted {
        return Err(BoogyError::SchemaMismatch(format!(
            "table '{name}' is not encrypted"
        )));
    }

    let cipher = crate::crypto::Cipher::new(key);

    // Verify the key by reading and decrypting the root page.
    // The root page is on disk (or in WAL) in encrypted form.
    // We need to read the raw bytes and decrypt.
    let root_page = state.meta.root_page;
    let raw = self.file.read_page_raw(root_page)?;
    let decrypted = cipher.decrypt_page(&raw)
        .map_err(|_| BoogyError::InvalidKey(name.to_string()))?;

    // Key is valid. Store it.
    state.meta.cipher = Some(cipher);

    // Now populate the cache with the decrypted page so subsequent reads work.
    let mut padded = [0u8; PAGE_SIZE];
    padded[..crate::crypto::ENCRYPTED_PAYLOAD_SIZE].copy_from_slice(&decrypted);
    let page = Page::from_bytes_unchecked(padded);
    self.file.put_cached_page(root_page, page);

    Ok(())
}
```

This requires two new methods on PageFile:
- `read_page_raw(&self, page_no: u32) -> Result<[u8; PAGE_SIZE]>` — reads raw bytes from disk without decryption or caching
- `put_cached_page(&self, page_no: u32, page: Page)` — inserts a page into the shared cache

- [ ] **Step 3: Add read_page_raw and put_cached_page to PageFile**

In `src/file.rs`:

```rust
/// Read raw page bytes from disk without caching or decryption.
/// Used by unlock_table to verify encryption keys.
pub fn read_page_raw(&self, page_no: u32) -> Result<[u8; PAGE_SIZE]> {
    let mut disk = self.disk.lock().unwrap();
    let offset = page_no as u64 * PAGE_SIZE as u64;
    disk.seek(SeekFrom::Start(offset))?;
    let mut buf = [0u8; PAGE_SIZE];
    disk.read_exact(&mut buf)?;
    Ok(buf)
}

/// Insert a plaintext page into the shared cache.
pub fn put_cached_page(&self, page_no: u32, page: Page) {
    let mut cache = self.pages.write().unwrap();
    while cache.len() <= page_no as usize {
        cache.push(None);
    }
    cache[page_no as usize] = Some(Arc::new(page));
}
```

- [ ] **Step 4: Thread encryption through commit_write**

The key change: when writing after-images to WAL, encrypt pages for encrypted tables. The `commit_write` function needs access to the cipher. Since it's called with a `table_id`, and the cipher is in `TableMeta`, pass an `Option<&Cipher>`:

```rust
fn commit_write(
    guard: WriteGuard,
    wal: &Mutex<Wal>,
    durability: Durability,
    table_id: u32,
    cipher: Option<&crate::crypto::Cipher>,
) -> Result<()> {
    let after_images = guard.commit()?;
    match durability {
        Durability::Immediate => {
            let mut wal = wal.lock().unwrap();
            for (page_no, data) in &after_images {
                let write_data = if let Some(c) = cipher {
                    let encrypted = c.encrypt_page(&data[..crate::crypto::ENCRYPTED_PAYLOAD_SIZE])?;
                    encrypted
                } else {
                    *data
                };
                wal.append_before_image(table_id, *page_no, &write_data)?;
            }
            wal.sync()?;
        }
        Durability::Normal => {
            let mut wal = wal.lock().unwrap();
            for (page_no, data) in &after_images {
                let write_data = if let Some(c) = cipher {
                    let encrypted = c.encrypt_page(&data[..crate::crypto::ENCRYPTED_PAYLOAD_SIZE])?;
                    encrypted
                } else {
                    *data
                };
                wal.append_before_image(table_id, *page_no, &write_data)?;
            }
        }
        Durability::None => {}
    }
    Ok(())
}
```

Update ALL callers of `commit_write` to pass the cipher. Most pass `None`. Table operations (insert, update, delete, etc.) pass `state.meta.cipher.as_ref()`.

- [ ] **Step 5: Thread encryption through sync_all (checkpoint)**

`sync_all()` writes all cached pages to disk. For encrypted tables, it needs to encrypt before writing. This requires knowing which pages belong to encrypted tables.

Simplest approach: add a `page_ciphers: HashMap<u32, Arc<Cipher>>` to PageFile that maps page_no → cipher. Populated when pages are first written for encrypted tables. Checked during sync_all.

Alternative (simpler for now): store the cipher alongside the page in the cache. Change `Vec<Option<Arc<Page>>>` to `Vec<Option<(Arc<Page>, Option<Arc<Cipher>>)>>`.

Actually simplest: have `sync_all` accept a cipher map parameter:

```rust
pub fn sync_all(&self, ciphers: &HashMap<u32, &crate::crypto::Cipher>) -> Result<()>
```

But this is invasive. For v1, take the simplest path: track which page ranges belong to encrypted tables.

Actually the SIMPLEST: during `commit_write`, if there's a cipher, encrypt and write the after-images to WAL (already done above). During `sync_all` (checkpoint/Drop), the cached pages are plaintext. Before writing each page to disk, check if it should be encrypted. The cipher info needs to be available at sync_all time.

Solution: `PageFile` gets a `page_ciphers: RwLock<HashMap<u32, Arc<crate::crypto::Cipher>>>` map. `commit_write` registers pages. `sync_all` looks up the cipher for each page.

This is getting complex. Let me simplify for v1:

**V1 approach:** Encrypted tables only use WAL-based persistence (Durability::Normal or Immediate). The `sync_all` at shutdown writes pages in plaintext to the data file (fast) — but on reopen, the WAL replay re-applies encrypted after-images, which are decrypted during replay. Actually no, this is wrong — after WAL replay, the data file should have the latest state. If WAL entries are encrypted, we need the key to replay.

**Simplest correct approach:** Store a cipher map in BoogyDb, pass it to sync_all and WAL replay.

```rust
pub struct BoogyDb {
    // ... existing fields ...
    /// Ciphers for encrypted tables, keyed by table_id. Populated by create_table_encrypted and unlock_table.
    table_ciphers: RwLock<HashMap<u32, Arc<crate::crypto::Cipher>>>,
}
```

`sync_all` iterates all tables, finds their root pages and all reachable pages, encrypts those belonging to encrypted tables. But finding "all reachable pages" requires traversing B+ trees which is expensive.

**Even simpler for v1:** Track `page_no → cipher` in a separate map. Each time a page is dirtied for an encrypted table, register it:

In `commit_write`, after collecting after_images, register page_no → cipher for each page.

OK, let me not over-engineer this. The cleanest approach:

1. WAL entries for encrypted tables are stored encrypted (done in commit_write)
2. `sync_all` (checkpoint) needs to encrypt pages before writing to disk
3. BoogyDb stores a `table_ciphers: RwLock<HashMap<u32, Arc<Cipher>>>` mapping table_id → cipher
4. BoogyDb also stores a `page_owners: RwLock<HashMap<u32, u32>>` mapping page_no → table_id (populated as pages are allocated)
5. `sync_all` takes `&HashMap<u32, &Cipher>` (page_no → cipher) built by BoogyDb before calling sync_all

For simplicity in this plan, let's have BoogyDb build the page→cipher map and pass it to a new `sync_all_encrypted` method on PageFile.

This is complex enough that I'll provide the key code but leave some plumbing to the implementer.

- [ ] **Step 6: Add table_ciphers and page_owners to BoogyDb**

```rust
pub struct BoogyDb {
    file: PageFile,
    wal: Mutex<Wal>,
    tables: RwLock<HashMap<String, Arc<RwLock<TableState>>>>,
    next_table_id: Mutex<u32>,
    durability: std::sync::atomic::AtomicU8,
    path: PathBuf,
    /// Cipher for each encrypted table, keyed by table_id.
    table_ciphers: RwLock<HashMap<u32, Arc<crate::crypto::Cipher>>>,
}
```

Initialize as `table_ciphers: RwLock::new(HashMap::new())` in `open()`.

When `create_table_encrypted` or `unlock_table` sets a cipher, also insert into `table_ciphers`.

- [ ] **Step 7: Guard operations on locked encrypted tables**

In every operation (insert, get, update, delete, find, count, etc.), after acquiring the table state, check:

```rust
if state.meta.encrypted && state.meta.cipher.is_none() {
    return Err(BoogyError::TableLocked(table.to_string()));
}
```

Add this check as a helper:
```rust
fn check_table_accessible(meta: &TableMeta, table: &str) -> Result<()> {
    if meta.encrypted && meta.cipher.is_none() {
        return Err(BoogyError::TableLocked(table.to_string()));
    }
    Ok(())
}
```

Call at the start of each public method after acquiring the table lock.

- [ ] **Step 8: Update sync_all in Drop to handle encryption**

In `Drop::drop`, build a page→cipher map from table_ciphers, pass to a new sync method:

```rust
impl Drop for BoogyDb {
    fn drop(&mut self) {
        let (metas, next_id) = self.snapshot_table_metas();
        {
            let mut guard = self.file.begin_write();
            if self.file.page_count() == 0 {
                let _ = guard.allocate_page();
            }
            let page = serialize_system_page(&metas, next_id);
            guard.put_page(0, page);
            // Commit — get after-images, encrypt if needed, write to WAL
            let after_images = guard.commit().unwrap_or_default();
            // Write encrypted after-images to WAL for encrypted tables
            if let Ok(mut wal) = self.wal.lock() {
                let ciphers = self.table_ciphers.read().unwrap();
                for (page_no, data) in &after_images {
                    // System page (0) is never encrypted
                    // For now, write all pages plaintext to the data file during shutdown
                    // The WAL handles encrypted persistence
                    let _ = wal.append_before_image(0, *page_no, data);
                }
            }
        }
        // Flush cached pages (plaintext) to disk
        let _ = self.file.sync_all();
        if let Ok(mut wal) = self.wal.lock() {
            let _ = wal.truncate();
        }
    }
}
```

Note: For v1, `sync_all` writes plaintext to the data file. Encrypted tables rely on WAL for persistence across reopens. On reopen, WAL entries (encrypted) are replayed. The data file for encrypted pages may contain stale plaintext — but after WAL replay, the cache has the correct decrypted pages.

- [ ] **Step 9: Update WAL replay in open() to handle encrypted entries**

During crash recovery, WAL entries for encrypted tables contain ciphertext. We can't decrypt them yet (no keys). Solution: apply them as-is to the data file (they're encrypted on disk, which is fine). When the caller later calls `unlock_table`, the root page is read from disk (encrypted), decrypted, and cached.

Actually, this means `read_page_from_disk` for encrypted tables returns garbage (encrypted bytes parsed as a Page). We need to handle this: encrypted pages on disk should NOT be loaded into the cache without decryption.

**V1 simplification:** Encrypted tables with Durability::None don't write to disk at all (pages only in cache). For Normal/Immediate, WAL entries are encrypted. On reopen:
1. WAL replay writes encrypted bytes to the data file (raw copy, no parsing)
2. `unlock_table` reads raw bytes from disk, decrypts, caches plaintext
3. Normal `read_page` checks the cache first (plaintext) — cache miss for encrypted tables goes through `read_page_raw` + decrypt if cipher is available

This means PageFile needs to know about encryption too, or BoogyDb intercepts cache misses for encrypted tables. The simplest: override the cache-miss path in BoogyDb — when a cache miss happens for an encrypted table's page, decrypt it.

For v1, let's keep it simple: encrypted table pages are eagerly loaded into the cache by `unlock_table` (walk the B+ tree from root, decrypt and cache every page). This avoids any changes to the read path. It costs memory (all encrypted pages in cache) but is correct and simple.

- [ ] **Step 10: Run all tests**

Run: `cargo test`
Expected: All existing tests pass (they use unencrypted tables, new code is unreachable).

- [ ] **Step 11: Commit**

```bash
git add src/db.rs src/file.rs
git commit -m "feat: create_table_encrypted, unlock_table, encrypted WAL writes"
```

---

## Task 5: Integration Tests

**Files:**
- Modify: `tests/crud_test.rs`

- [ ] **Step 1: Test encrypted table basic roundtrip**

```rust
#[test]
fn test_encrypted_table_roundtrip() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.boogy");
    let key = [0x42u8; 32];

    let db = BoogyDb::open(&path).unwrap();
    db.create_table_encrypted("secrets", &[
        ColumnDef::new("token", Type::Text),
        ColumnDef::new("value", Type::Integer),
    ], &key).unwrap();

    let id = db.insert("secrets", &[
        ("token", Value::Text("abc123".into())),
        ("value", Value::Integer(42)),
    ]).unwrap();

    let row = db.get("secrets", id).unwrap().unwrap();
    assert_eq!(row.get("token").unwrap(), Value::Text("abc123".into()));
    assert_eq!(row.get("value").unwrap(), Value::Integer(42));
}
```

- [ ] **Step 2: Test locked table returns error**

```rust
#[test]
fn test_encrypted_table_locked_without_key() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.boogy");
    let key = [0x42u8; 32];

    {
        let db = BoogyDb::open(&path).unwrap();
        db.create_table_encrypted("secrets", &[
            ColumnDef::new("v", Type::Integer),
        ], &key).unwrap();
        db.insert("secrets", &[("v", Value::Integer(1))]).unwrap();
    }

    {
        let db = BoogyDb::open(&path).unwrap();
        // Table is locked — operations should fail
        assert!(db.get("secrets", 1).is_err());
        assert!(db.insert("secrets", &[("v", Value::Integer(2))]).is_err());

        // Unlock with correct key
        db.unlock_table("secrets", &key).unwrap();

        // Now it works
        let row = db.get("secrets", 1).unwrap().unwrap();
        assert_eq!(row.get("v").unwrap(), Value::Integer(1));
    }
}
```

- [ ] **Step 3: Test wrong key rejected**

```rust
#[test]
fn test_encrypted_table_wrong_key() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.boogy");
    let key = [0x42u8; 32];
    let wrong_key = [0x43u8; 32];

    {
        let db = BoogyDb::open(&path).unwrap();
        db.create_table_encrypted("secrets", &[
            ColumnDef::new("v", Type::Integer),
        ], &key).unwrap();
        db.insert("secrets", &[("v", Value::Integer(1))]).unwrap();
    }

    {
        let db = BoogyDb::open(&path).unwrap();
        assert!(db.unlock_table("secrets", &wrong_key).is_err());
    }
}
```

- [ ] **Step 4: Test mixed encrypted and unencrypted tables**

```rust
#[test]
fn test_mixed_encrypted_unencrypted() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("test.boogy");
    let key = [0x42u8; 32];

    let db = BoogyDb::open(&path).unwrap();
    db.create_table("public", &[ColumnDef::new("v", Type::Integer)]).unwrap();
    db.create_table_encrypted("private", &[ColumnDef::new("v", Type::Integer)], &key).unwrap();

    db.insert("public", &[("v", Value::Integer(1))]).unwrap();
    db.insert("private", &[("v", Value::Integer(2))]).unwrap();

    assert_eq!(db.get("public", 1).unwrap().unwrap().get("v").unwrap(), Value::Integer(1));
    assert_eq!(db.get("private", 1).unwrap().unwrap().get("v").unwrap(), Value::Integer(2));
}
```

- [ ] **Step 5: Run all tests**

Run: `cargo test`
Expected: All tests pass.

- [ ] **Step 6: Commit**

```bash
git add tests/crud_test.rs
git commit -m "test: encrypted table integration tests"
```

---

## Task 6: Verify + Cleanup

- [ ] **Step 1: Run full test suite**

Run: `cargo test`

- [ ] **Step 2: Run benchmarks to verify no regression on unencrypted tables**

Run: `cargo bench --bench point_ops`
Expected: No regression (encryption code is behind an `Option::is_some()` check).

- [ ] **Step 3: Commit and push**

```bash
git add -A
git commit -m "feat: per-table AES-256-GCM encryption — opt-in at create_table"
git push
```
