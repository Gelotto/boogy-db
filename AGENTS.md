# Agent Workflow Guide

## Before Committing

1. Run `cargo test` and verify all tests pass.
2. Run `cargo clippy` and fix any warnings.
3. If you changed performance-critical code, run the relevant benchmark before and after to verify no regression.

## Branch Naming

- `feat/<description>` -- new features or public API additions
- `perf/<description>` -- performance optimizations
- `fix/<description>` -- bug fixes

## Commit Messages

Format: `<type>: <description>` where type is `feat`, `perf`, `fix`, `refactor`, `test`, `bench`, or `docs`. Keep the first line under 72 characters.

## Parallel Safety

**Safe to modify in parallel** (independent modules):
- `crypto.rs` -- isolated encryption logic
- `error.rs`, `value.rs` -- type definitions only
- `filter.rs` -- filter evaluation, no shared state
- Individual benchmark files in `benches/`
- Individual test files in `tests/`

**Requires coordination** (tightly coupled):
- `db.rs` + `table.rs` -- public API and table metadata are intertwined
- `btree.rs` + `page.rs` -- B+ tree layout depends on page header format
- `btree.rs` + `row.rs` -- tree operations use row encoding functions
- `index.rs` + `btree.rs` -- index tree mirrors the B+ tree structure
- `file.rs` + `wal.rs` -- WriteGuard commit path feeds into WAL

## Verifying Benchmark Performance

```bash
# Run before your change
cargo bench --bench <name> 2>&1 | tee /tmp/bench-before.txt

# Apply your change, then run again
cargo bench --bench <name> 2>&1 | tee /tmp/bench-after.txt

# Compare results manually -- look for >5% regressions
diff /tmp/bench-before.txt /tmp/bench-after.txt
```

Key benchmarks: `point_ops` (insert/get latency), `profile_ops` (mixed workload), `concurrent_ops` (multi-threaded), `sqlite_comparison` (vs SQLite).
