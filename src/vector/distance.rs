/// Cosine distance: 1 - cosine_similarity.
///
/// Range [0, 2]. Zero vector returns 1.0.
pub fn cosine_distance(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len(), "vector dimension mismatch");
    let mut dot = 0.0f32;
    let mut norm_a = 0.0f32;
    let mut norm_b = 0.0f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        norm_a += a[i] * a[i];
        norm_b += b[i] * b[i];
    }
    let denom = norm_a.sqrt() * norm_b.sqrt();
    if denom == 0.0 {
        return 1.0;
    }
    1.0 - (dot / denom)
}

/// Euclidean (L2) distance, squared.
///
/// Returns squared L2 distance (no sqrt on hot path). Range [0, inf).
pub fn euclidean_distance(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len(), "vector dimension mismatch");
    let mut sum = 0.0f32;
    for i in 0..a.len() {
        let diff = a[i] - b[i];
        sum += diff * diff;
    }
    sum
}

/// Negative dot product distance.
///
/// Returns negative dot product so lower = more similar, consistent with other metrics.
/// Range (-inf, inf).
pub fn dot_product_distance(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len(), "vector dimension mismatch");
    let mut dot = 0.0f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
    }
    -dot
}

// ---------------------------------------------------------------------------
// AVX2 SIMD implementations
// ---------------------------------------------------------------------------

#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn hsum_avx2(v: std::arch::x86_64::__m256) -> f32 {
    use std::arch::x86_64::*;
    unsafe {
        let hi = _mm256_extractf128_ps(v, 1);
        let lo = _mm256_castps256_ps128(v);
        let sum128 = _mm_add_ps(lo, hi);
        let shuf = _mm_movehdup_ps(sum128);
        let sums = _mm_add_ps(sum128, shuf);
        let shuf2 = _mm_movehl_ps(sums, sums);
        let result = _mm_add_ss(sums, shuf2);
        _mm_cvtss_f32(result)
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn cosine_distance_avx2(a: &[f32], b: &[f32]) -> f32 {
    use std::arch::x86_64::*;
    debug_assert_eq!(a.len(), b.len(), "vector dimension mismatch");

    let len = a.len();

    unsafe {
        let mut dot_acc = _mm256_setzero_ps();
        let mut norm_a_acc = _mm256_setzero_ps();
        let mut norm_b_acc = _mm256_setzero_ps();

        let mut i = 0;
        while i + 8 <= len {
            let va = _mm256_loadu_ps(a.as_ptr().add(i));
            let vb = _mm256_loadu_ps(b.as_ptr().add(i));
            dot_acc = _mm256_add_ps(dot_acc, _mm256_mul_ps(va, vb));
            norm_a_acc = _mm256_add_ps(norm_a_acc, _mm256_mul_ps(va, va));
            norm_b_acc = _mm256_add_ps(norm_b_acc, _mm256_mul_ps(vb, vb));
            i += 8;
        }

        let mut dot = hsum_avx2(dot_acc);
        let mut norm_a = hsum_avx2(norm_a_acc);
        let mut norm_b = hsum_avx2(norm_b_acc);

        // Tail loop for remaining elements
        while i < len {
            dot += a[i] * b[i];
            norm_a += a[i] * a[i];
            norm_b += b[i] * b[i];
            i += 1;
        }

        let denom = norm_a.sqrt() * norm_b.sqrt();
        if denom == 0.0 {
            return 1.0;
        }
        1.0 - (dot / denom)
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn euclidean_distance_avx2(a: &[f32], b: &[f32]) -> f32 {
    use std::arch::x86_64::*;
    debug_assert_eq!(a.len(), b.len(), "vector dimension mismatch");

    let len = a.len();

    unsafe {
        let mut sum_acc = _mm256_setzero_ps();

        let mut i = 0;
        while i + 8 <= len {
            let va = _mm256_loadu_ps(a.as_ptr().add(i));
            let vb = _mm256_loadu_ps(b.as_ptr().add(i));
            let diff = _mm256_sub_ps(va, vb);
            sum_acc = _mm256_add_ps(sum_acc, _mm256_mul_ps(diff, diff));
            i += 8;
        }

        let mut sum = hsum_avx2(sum_acc);

        while i < len {
            let diff = a[i] - b[i];
            sum += diff * diff;
            i += 1;
        }

        sum
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn dot_product_distance_avx2(a: &[f32], b: &[f32]) -> f32 {
    use std::arch::x86_64::*;
    debug_assert_eq!(a.len(), b.len(), "vector dimension mismatch");

    let len = a.len();

    unsafe {
        let mut sum_acc = _mm256_setzero_ps();

        let mut i = 0;
        while i + 8 <= len {
            let va = _mm256_loadu_ps(a.as_ptr().add(i));
            let vb = _mm256_loadu_ps(b.as_ptr().add(i));
            sum_acc = _mm256_add_ps(sum_acc, _mm256_mul_ps(va, vb));
            i += 8;
        }

        let mut dot = hsum_avx2(sum_acc);

        while i < len {
            dot += a[i] * b[i];
            i += 1;
        }

        -dot
    }
}

// Safe wrappers for AVX2 functions (needed because #[target_feature] fns are unsafe).

#[cfg(target_arch = "x86_64")]
fn cosine_distance_avx2_wrapper(a: &[f32], b: &[f32]) -> f32 {
    unsafe { cosine_distance_avx2(a, b) }
}

#[cfg(target_arch = "x86_64")]
fn euclidean_distance_avx2_wrapper(a: &[f32], b: &[f32]) -> f32 {
    unsafe { euclidean_distance_avx2(a, b) }
}

#[cfg(target_arch = "x86_64")]
fn dot_product_distance_avx2_wrapper(a: &[f32], b: &[f32]) -> f32 {
    unsafe { dot_product_distance_avx2(a, b) }
}

/// Returns the distance function for the given metric.
pub fn distance_fn(metric: super::types::DistanceMetric) -> fn(&[f32], &[f32]) -> f32 {
    use super::types::DistanceMetric;

    #[cfg(target_arch = "x86_64")]
    {
        if std::is_x86_feature_detected!("avx2") {
            return match metric {
                DistanceMetric::Cosine => cosine_distance_avx2_wrapper,
                DistanceMetric::Euclidean => euclidean_distance_avx2_wrapper,
                DistanceMetric::DotProduct => dot_product_distance_avx2_wrapper,
            };
        }
    }

    match metric {
        DistanceMetric::Cosine => cosine_distance,
        DistanceMetric::Euclidean => euclidean_distance,
        DistanceMetric::DotProduct => dot_product_distance,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::types::DistanceMetric;

    // cosine_distance tests

    #[test]
    fn cosine_identical() {
        let v = [1.0f32, 2.0, 3.0];
        assert!((cosine_distance(&v, &v) - 0.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_orthogonal() {
        let a = [1.0f32, 0.0];
        let b = [0.0f32, 1.0];
        assert!((cosine_distance(&a, &b) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_opposite() {
        let a = [1.0f32, 0.0];
        let b = [-1.0f32, 0.0];
        assert!((cosine_distance(&a, &b) - 2.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_zero_vector() {
        let a = [0.0f32, 0.0];
        let b = [1.0f32, 0.0];
        assert_eq!(cosine_distance(&a, &b), 1.0);
    }

    // euclidean_distance tests

    #[test]
    fn euclidean_identical() {
        let v = [1.0f32, 2.0, 3.0];
        assert_eq!(euclidean_distance(&v, &v), 0.0);
    }

    #[test]
    fn euclidean_known_value() {
        // [0,0] to [3,4]: actual L2 = 5, squared = 25
        let a = [0.0f32, 0.0];
        let b = [3.0f32, 4.0];
        assert!((euclidean_distance(&a, &b) - 25.0).abs() < 1e-6);
    }

    // dot_product_distance tests

    #[test]
    fn dot_product_identical() {
        let v = [1.0f32, 2.0, 3.0];
        // dot([1,2,3],[1,2,3]) = 14, negated = -14
        assert!((dot_product_distance(&v, &v) - (-14.0)).abs() < 1e-6);
    }

    #[test]
    fn dot_product_orthogonal() {
        let a = [1.0f32, 0.0];
        let b = [0.0f32, 1.0];
        assert_eq!(dot_product_distance(&a, &b), 0.0);
    }

    // distance_fn dispatch tests

    #[test]
    fn dispatch_cosine() {
        let f = distance_fn(DistanceMetric::Cosine);
        let v = [1.0f32, 0.0];
        assert!((f(&v, &v) - 0.0).abs() < 1e-6);
    }

    #[test]
    fn dispatch_euclidean() {
        let f = distance_fn(DistanceMetric::Euclidean);
        let a = [0.0f32, 0.0];
        let b = [3.0f32, 4.0];
        assert!((f(&a, &b) - 25.0).abs() < 1e-6);
    }

    #[test]
    fn dispatch_dot_product() {
        let f = distance_fn(DistanceMetric::DotProduct);
        let a = [1.0f32, 0.0];
        let b = [0.0f32, 1.0];
        assert_eq!(f(&a, &b), 0.0);
    }

    // NaN handling tests.
    //
    // NaN propagates through IEEE 754 arithmetic, so any NaN in the input
    // produces NaN output. This is "garbage in, garbage out" — callers must
    // validate inputs before insertion. We document this here so that if
    // NaN-guarding is added later, these tests catch the behavior change.

    #[test]
    fn nan_propagates_in_euclidean() {
        let a = [1.0f32, f32::NAN, 3.0];
        let b = [1.0f32, 2.0, 3.0];
        let d = euclidean_distance(&a, &b);
        assert!(d.is_nan(), "euclidean_distance with NaN input should return NaN, got {d}");
    }

    #[test]
    fn nan_propagates_in_cosine() {
        let a = [1.0f32, f32::NAN, 0.0];
        let b = [1.0f32, 0.0, 0.0];
        let d = cosine_distance(&a, &b);
        assert!(d.is_nan(), "cosine_distance with NaN input should return NaN, got {d}");
    }

    #[test]
    fn nan_propagates_in_dot_product() {
        let a = [f32::NAN, 2.0];
        let b = [1.0f32, 2.0];
        let d = dot_product_distance(&a, &b);
        assert!(d.is_nan(), "dot_product_distance with NaN input should return NaN, got {d}");
    }

    // cosine on normalized vectors equals 1 - dot_product
    #[test]
    fn cosine_normalized_equals_one_minus_dot() {
        // Normalize [3,4] to a unit vector
        let mag = (3.0f32 * 3.0 + 4.0 * 4.0).sqrt();
        let a = [3.0f32 / mag, 4.0 / mag];

        // Normalize [1,0] — already unit
        let b = [1.0f32, 0.0];

        let cos_d = cosine_distance(&a, &b);

        let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
        let expected = 1.0 - dot;

        assert!((cos_d - expected).abs() < 1e-6);
    }

    // AVX2 SIMD tests

    #[cfg(target_arch = "x86_64")]
    mod avx2_tests {
        use super::*;

        fn has_avx2() -> bool {
            std::is_x86_feature_detected!("avx2")
        }

        #[test]
        fn avx2_cosine_matches_scalar() {
            if !has_avx2() { return; }
            let a: Vec<f32> = (0..128).map(|i| (i as f32) * 0.1).collect();
            let b: Vec<f32> = (0..128).map(|i| ((i * 3 + 7) as f32) * 0.05).collect();
            let scalar = cosine_distance(&a, &b);
            let simd = cosine_distance_avx2_wrapper(&a, &b);
            assert!((scalar - simd).abs() < 1e-5, "scalar={scalar}, simd={simd}");
        }

        #[test]
        fn avx2_euclidean_matches_scalar() {
            if !has_avx2() { return; }
            let a: Vec<f32> = (0..128).map(|i| (i as f32) * 0.1).collect();
            let b: Vec<f32> = (0..128).map(|i| ((i * 3 + 7) as f32) * 0.05).collect();
            let scalar = euclidean_distance(&a, &b);
            let simd = euclidean_distance_avx2_wrapper(&a, &b);
            assert!((scalar - simd).abs() < 1e-2, "scalar={scalar}, simd={simd}");
        }

        #[test]
        fn avx2_dot_product_matches_scalar() {
            if !has_avx2() { return; }
            let a: Vec<f32> = (0..128).map(|i| (i as f32) * 0.1).collect();
            let b: Vec<f32> = (0..128).map(|i| ((i * 3 + 7) as f32) * 0.05).collect();
            let scalar = dot_product_distance(&a, &b);
            let simd = dot_product_distance_avx2_wrapper(&a, &b);
            assert!((scalar - simd).abs() < 1e-2, "scalar={scalar}, simd={simd}");
        }

        #[test]
        fn avx2_cosine_odd_dimensions() {
            if !has_avx2() { return; }
            for dims in [1, 3, 7, 13, 15, 31, 33] {
                let a: Vec<f32> = (0..dims).map(|i| (i as f32) * 0.1 + 0.5).collect();
                let b: Vec<f32> = (0..dims).map(|i| (i as f32) * 0.2 + 0.3).collect();
                let scalar = cosine_distance(&a, &b);
                let simd = cosine_distance_avx2_wrapper(&a, &b);
                assert!((scalar - simd).abs() < 1e-5,
                    "dims={dims}: scalar={scalar}, simd={simd}");
            }
        }

        #[test]
        fn avx2_euclidean_odd_dimensions() {
            if !has_avx2() { return; }
            for dims in [1, 3, 7, 13, 15, 31, 33] {
                let a: Vec<f32> = (0..dims).map(|i| (i as f32) * 0.1 + 0.5).collect();
                let b: Vec<f32> = (0..dims).map(|i| (i as f32) * 0.2 + 0.3).collect();
                let scalar = euclidean_distance(&a, &b);
                let simd = euclidean_distance_avx2_wrapper(&a, &b);
                assert!((scalar - simd).abs() < 1e-3,
                    "dims={dims}: scalar={scalar}, simd={simd}");
            }
        }

        #[test]
        fn avx2_dot_product_odd_dimensions() {
            if !has_avx2() { return; }
            for dims in [1, 3, 7, 13, 15, 31, 33] {
                let a: Vec<f32> = (0..dims).map(|i| (i as f32) * 0.1 + 0.5).collect();
                let b: Vec<f32> = (0..dims).map(|i| (i as f32) * 0.2 + 0.3).collect();
                let scalar = dot_product_distance(&a, &b);
                let simd = dot_product_distance_avx2_wrapper(&a, &b);
                assert!((scalar - simd).abs() < 1e-3,
                    "dims={dims}: scalar={scalar}, simd={simd}");
            }
        }

        #[test]
        fn dispatcher_returns_avx2() {
            if !has_avx2() { return; }
            let f = distance_fn(DistanceMetric::Euclidean);
            let a = vec![1.0f32; 32];
            let b = vec![2.0f32; 32];
            assert_eq!(f(&a, &b), euclidean_distance_avx2_wrapper(&a, &b));
        }
    }
}
