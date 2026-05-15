# SIMD Distance Functions Design

AVX2-accelerated distance functions for boogy-db's vector search. Replaces scalar loops with 8-wide SIMD operations on x86-64 CPUs that support AVX2. Automatic at runtime — no opt-in, no feature flags.

## Approach

Runtime CPU feature detection via `std::is_x86_feature_detected!("avx2")`. The `distance_fn()` dispatcher checks once per collection and returns either the AVX2 or scalar function pointer. Same `fn(&[f32], &[f32]) -> f32` signature — callers are unaware of which path they got.

On x86-64 with AVX2 (virtually all servers/desktops from 2013+): SIMD is the default.
On non-x86 platforms or rare x86 CPUs without AVX2: scalar fallback.

## Implementation

Three new functions in `src/vector/distance.rs`, each annotated with `#[target_feature(enable = "avx2")]` and marked `unsafe`:

### `cosine_distance_avx2`

Process 8 floats per iteration using `__m256` registers:
- `_mm256_loadu_ps` to load 8 floats from each vector
- `_mm256_mul_ps` for element-wise multiply (dot product terms)
- `_mm256_fmadd_ps` for fused multiply-add (norm accumulation) if FMA available, otherwise `_mm256_add_ps` + `_mm256_mul_ps`
- Horizontal sum of the 8-lane accumulator at the end via `_mm256_hadd_ps` + extract
- Tail loop for remainder elements (dims % 8 != 0)
- Same zero-vector guard: `denom == 0.0 → 1.0`

### `euclidean_distance_avx2`

- `_mm256_sub_ps` for `a - b`
- `_mm256_mul_ps` for squaring
- `_mm256_add_ps` to accumulate
- Horizontal sum + tail loop

### `dot_product_distance_avx2`

- `_mm256_mul_ps` for `a * b`
- `_mm256_add_ps` to accumulate
- Horizontal sum + negate + tail loop

## Dispatcher

`distance_fn()` updated:

```rust
pub fn distance_fn(metric: DistanceMetric) -> fn(&[f32], &[f32]) -> f32 {
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") {
            return match metric {
                DistanceMetric::Cosine => cosine_distance_avx2_wrapper,
                DistanceMetric::Euclidean => euclidean_distance_avx2_wrapper,
                DistanceMetric::DotProduct => dot_product_distance_avx2_wrapper,
            };
        }
    }
    // Scalar fallback
    match metric {
        DistanceMetric::Cosine => cosine_distance,
        DistanceMetric::Euclidean => euclidean_distance,
        DistanceMetric::DotProduct => dot_product_distance,
    }
}
```

Each `*_avx2_wrapper` is a safe `fn` that calls the `unsafe` target_feature function. This is safe because the dispatcher already verified AVX2 support.

## Safety

- `#[target_feature(enable = "avx2")]` functions are `unsafe` because calling them on CPUs without AVX2 is UB.
- The wrappers are only returned by the dispatcher after `is_x86_feature_detected!("avx2")` succeeds.
- `_mm256_loadu_ps` is used (unaligned load) since vectors in the mmap may not be 32-byte aligned.

## No New Dependencies

Uses `std::arch::x86_64` intrinsics directly. No `packed_simd`, no `simdeez`, no external crates.

## Testing

- Existing distance tests continue to run (they test whatever `distance_fn` returns — which is now AVX2 on x86).
- Add explicit tests that call the AVX2 functions directly (via the wrappers) with the same test vectors, verifying identical results to scalar within f32 epsilon.
- Test with dimensions not divisible by 8 (e.g., 3, 7, 13) to exercise the tail loop.
- Test with very small vectors (1, 2 elements) — edge case for the SIMD path.

## Benchmarks

Add to the existing `benches/vector_ops.rs`:
- Scalar vs AVX2 comparison at dims 128, 768, 1536
- Raw distance function throughput (millions of distance computations per second)

Expected speedup: 4-6x for euclidean/dot product, 3-5x for cosine (more dependent operations in the accumulation).
