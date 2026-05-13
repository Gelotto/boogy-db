# Configuring boogy-db

How to configure durability, ACID mode, and encryption for your use case.

## Opening a Database

```rust
use boogy_db::{BoogyDb, Durability};

let db = BoogyDb::open("app.boogy")?;
```

The file is created if it doesn't exist. The database flushes to disk and truncates the WAL on drop.

## Durability Modes

Set after opening with `db.set_durability(...)`. Controls how writes are persisted.

| Mode | Behavior | Survives |
|------|----------|----------|
| `Durability::Immediate` | fsync WAL on every commit | Power loss |
| `Durability::Normal` | Write WAL, no fsync | Process crash (OS caches data) |
| `Durability::None` | No WAL writes | Nothing -- fastest, data is memory-only until shutdown |

```rust
// Financial data -- cannot lose a single transaction
db.set_durability(Durability::Immediate);

// Most applications -- good balance of safety and speed
db.set_durability(Durability::Normal);

// Ephemeral cache, scratch data, or tests
db.set_durability(Durability::None);
```

**Default**: `Durability::Normal`.

**Read back**: `db.durability()` returns the current setting.

## ACID Mode

ACID mode gives true all-or-nothing semantics for transactions that touch multiple tables. Without it, `begin()`/`commit()` uses lightweight transactions where each operation commits independently.

```rust
// Enable ACID for multi-table consistency
db.set_acid(true);

let mut tx = db.begin()?;
tx.insert("orders", &[("total", Value::Integer(100))])?;
tx.insert("ledger", &[("amount", Value::Integer(-100))])?;
tx.commit()?;  // Both writes become visible atomically
// If commit() is never called (e.g., error/panic), both are rolled back
```

**Trade-off**: ACID transactions hold a private dirty page buffer and replay on commit. This adds memory overhead proportional to the number of pages modified. For single-table operations, leave ACID off.

```rust
db.is_acid(); // check current setting
```

## Encryption

Encrypt individual tables at rest with AES-256-GCM. Plaintext lives in memory; encryption happens only at disk I/O.

```rust
// Create an encrypted table with a 32-byte key
let key: [u8; 32] = rand::random();
db.create_table_encrypted("secrets", &[
    ColumnDef::new("name", Type::Text),
    ColumnDef::new("value", Type::Blob),
], &key)?;

// After reopening the database, unlock before use
let db = BoogyDb::open("app.boogy")?;
db.unlock_table("secrets", &key)?;
```

**Key management**: Store keys outside the database file -- in environment variables, a vault service, or a separate keyfile. Never store the key alongside the `.boogy` file.

**Performance impact**: Encryption adds overhead only during `open()` (WAL replay decryption) and shutdown (page flush encryption). In-memory operations are unaffected since the page cache holds plaintext.

**Locked table behavior**: Operations on an encrypted table that hasn't been unlocked return `BoogyError::TableLocked`.

## Example Configurations

### Web API backend
```rust
let db = BoogyDb::open("api.boogy")?;
db.set_durability(Durability::Immediate); // can't lose data
db.set_acid(true);                        // multi-table consistency
```

### CLI tool
```rust
let db = BoogyDb::open("data.boogy")?;
db.set_durability(Durability::Normal);    // crash-safe enough
// No ACID needed -- typically single-table operations
```

### Tests
```rust
let dir = tempfile::TempDir::new()?;
let db = BoogyDb::open(dir.path().join("test.boogy"))?;
db.set_durability(Durability::None);      // speed over safety
```

### Cache / ephemeral storage
```rust
let db = BoogyDb::open("/tmp/cache.boogy")?;
db.set_durability(Durability::None);
// Data is fast but may be lost on crash -- that's fine for a cache
```
