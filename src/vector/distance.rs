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

/// Returns the distance function for the given metric.
pub fn distance_fn(metric: super::types::DistanceMetric) -> fn(&[f32], &[f32]) -> f32 {
    use super::types::DistanceMetric;
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
}
