use crate::stats::percentiles::{bootstrap_from_total, PercentileSource};
use hdrhistogram::{AdditionError, CreationError, Histogram, RecordError};
use rand::rngs::SmallRng;

/// A histogram over signed values built from two HDR histograms.
///
/// HDR stores only non-negative integers, so non-negative values (zero
/// included) go to `positive` and magnitudes of negative values to `negative`
/// — a DDSketch-style sign split that keeps HDR's relative precision symmetric
/// around zero. It mirrors the `Histogram` interface the stats code uses, so a
/// recorder can hold it wherever it would hold a plain histogram.
#[derive(Clone, Debug)]
pub struct SignedHistogram {
    positive: Histogram<u64>,
    negative: Histogram<u64>,
}

impl SignedHistogram {
    /// Builds a SignedHistogram whose two stores both use `sigfig` significant
    /// figures of precision, like `Histogram::new`. Both stores share the
    /// precision so resolution stays symmetric around zero.
    pub fn new(sigfig: u8) -> Result<Self, CreationError> {
        Ok(Self {
            positive: Histogram::new(sigfig)?,
            negative: Histogram::new(sigfig)?,
        })
    }

    /// Records a value given as a magnitude and a sign. Returns hdr's `Result`
    /// like `Histogram::record`, leaving the propagate-or-unwrap choice to the
    /// owning recorder.
    pub fn record(&mut self, magnitude: u64, negative: bool) -> Result<(), RecordError> {
        if negative {
            self.negative.record(magnitude)
        } else {
            self.positive.record(magnitude)
        }
    }

    pub fn add(&mut self, other: &SignedHistogram) -> Result<(), AdditionError> {
        self.positive.add(&other.positive)?;
        self.negative.add(&other.negative)
    }

    pub fn clear(&mut self) {
        self.positive.clear();
        self.negative.clear();
    }

    /// Total number of recorded values.
    pub fn len(&self) -> u64 {
        self.positive.len() + self.negative.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Non-negative recorded values, zero included.
    pub fn positive(&self) -> &Histogram<u64> {
        &self.positive
    }

    /// Magnitudes of negative recorded values.
    pub fn negative(&self) -> &Histogram<u64> {
        &self.negative
    }

    /// Mean of all recorded values, combining both stores. With no negatives it
    /// reduces to the plain histogram's mean, up to floating-point rounding.
    pub fn mean(&self) -> f64 {
        if self.is_empty() {
            return 0.0;
        }
        let positive_n = self.positive.len() as f64;
        let negative_n = self.negative.len() as f64;
        (self.positive.mean() * positive_n - self.negative.mean() * negative_n)
            / (positive_n + negative_n)
    }

    /// Standard deviation of all recorded values, combining both stores.
    pub fn stdev(&self) -> f64 {
        if self.is_empty() {
            return 0.0;
        }
        let positive_n = self.positive.len() as f64;
        let negative_n = self.negative.len() as f64;
        let n = positive_n + negative_n;
        // Parallel-variance combine of the two groups (signed means +positive,
        // -negative). Stays stable instead of cancelling E[X^2] against E[X]^2,
        // which would understate or zero the variance for a large mean.
        let m2_positive = self.positive.stdev().powi(2) * positive_n;
        let m2_negative = self.negative.stdev().powi(2) * negative_n;
        let mean_gap = self.positive.mean() + self.negative.mean();
        let m2 = m2_positive + m2_negative + mean_gap * mean_gap * positive_n * negative_n / n;
        (m2 / n).sqrt()
    }

    /// Value at the given percentile, negative for values from the `negative`
    /// store. Follows the rank convention of `Histogram::value_at_percentile`
    /// (target rank `max(1, ceil(percentile/100 * total))`, low edge at the
    /// minimum and high edge elsewhere); with no negatives it reduces to exactly
    /// `Histogram::value_at_percentile`, which the tests pin across every
    /// percentile.
    pub fn value_at_percentile(&self, percentile: f64) -> f64 {
        let total = self.len();
        if total == 0 {
            return 0.0;
        }
        let quantile = (percentile / 100.0).min(1.0);
        let target = ((quantile * total as f64).ceil() as u64).clamp(1, total);

        let negative_count = self.negative.len();
        if target <= negative_count {
            // Negative values run opposite to magnitude order, so the target-th
            // smallest value is the (negative_count - target + 1)-th smallest
            // magnitude, found by walking magnitudes ascending.
            let magnitude_rank = negative_count - target + 1;
            let mut cumulative = 0;
            for v in self.negative.iter_recorded() {
                cumulative += v.count_since_last_iteration();
                if cumulative >= magnitude_rank {
                    let value = v.value_iterated_to();
                    return if quantile == 0.0 {
                        -(self.negative.highest_equivalent(value) as f64)
                    } else {
                        -(self.negative.lowest_equivalent(value) as f64)
                    };
                }
            }
            unreachable!("ranks up to negative.len() are covered by the negative store");
        }
        let mut cumulative = 0;
        for v in self.positive.iter_recorded() {
            cumulative += v.count_since_last_iteration();
            if cumulative >= target - negative_count {
                let value = v.value_iterated_to();
                return if quantile == 0.0 {
                    self.positive.lowest_equivalent(value) as f64
                } else {
                    self.positive.highest_equivalent(value) as f64
                };
            }
        }
        unreachable!("ranks above negative.len() are covered by the positive store");
    }
}

impl Default for SignedHistogram {
    fn default() -> Self {
        Self::new(3).unwrap()
    }
}

impl PercentileSource for SignedHistogram {
    fn value_at_percentile(&self, percentile: f64) -> f64 {
        SignedHistogram::value_at_percentile(self, percentile)
    }

    /// Resamples both stores against the combined value count, so the pair
    /// stays consistent with the original sign split.
    fn bootstrap(&self, rng: &mut SmallRng, effective_n: u64) -> Self {
        let total = self.len();
        SignedHistogram {
            negative: bootstrap_from_total(rng, &self.negative, total, effective_n),
            positive: bootstrap_from_total(rng, &self.positive, total, effective_n),
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::stats::percentiles::{Percentile, Percentiles};
    use assert_approx_eq::assert_approx_eq;
    use rand::{Rng, SeedableRng};
    use strum::IntoEnumIterator;

    /// Records a signed integer magnitude into the two-store histogram.
    fn record(hist: &mut SignedHistogram, v: i64) {
        hist.record(v.unsigned_abs(), v < 0).unwrap();
    }

    /// True when two values agree up to floating-point rounding (relative).
    fn approx_eq(a: f64, b: f64) -> bool {
        (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0)
    }

    /// value_at_percentile must pick the right rank, store, and bucket edge.
    /// The oracle independently sorts the raw values and applies the matching
    /// store's edge function, so a wrong store or edge is caught exactly rather
    /// than hidden by a tolerance. `n` spans integer ranks (100, 1000) and not.
    #[test]
    fn value_at_percentile_matches_per_store_oracle() {
        let mut rng = SmallRng::seed_from_u64(7);
        for &n in &[1usize, 2, 3, 10, 100, 1000, 1003] {
            let mut hist = SignedHistogram::default();
            let mut values: Vec<i64> = Vec::new();
            for _ in 0..n {
                let v: i64 = rng.random_range(-2_000_000..2_000_000);
                values.push(v);
                record(&mut hist, v);
            }
            values.sort();
            for p in Percentile::iter() {
                let quantile = (p.value() / 100.0).min(1.0);
                let target = ((quantile * n as f64).ceil() as usize).clamp(1, n);
                let v_t = values[target - 1];
                let expected = if v_t < 0 {
                    let mag = v_t.unsigned_abs();
                    if quantile == 0.0 {
                        -(hist.negative().highest_equivalent(mag) as f64)
                    } else {
                        -(hist.negative().lowest_equivalent(mag) as f64)
                    }
                } else if quantile == 0.0 {
                    hist.positive().lowest_equivalent(v_t as u64) as f64
                } else {
                    hist.positive().highest_equivalent(v_t as u64) as f64
                };
                assert_eq!(
                    hist.value_at_percentile(p.value()),
                    expected,
                    "n={n} percentile={}",
                    p.value(),
                );
            }
        }
    }

    /// At exact-integer ranks (quantile*total whole) the ceil convention makes
    /// p and 100-p select adjacent magnitudes, not mirror images — e.g. p10 is
    /// rank 1 (-500) but p90 is rank 9 (+400), not rank 10 (+500). This is the
    /// one-rank gap `symmetric_distribution_is_antisymmetric` avoids with a
    /// non-integer total. Unit-resolution magnitudes make the values exact.
    #[test]
    fn value_at_percentile_at_integer_ranks() {
        let mut hist = SignedHistogram::default();
        for m in [100u64, 200, 300, 400, 500] {
            hist.record(m, false).unwrap();
            hist.record(m, true).unwrap();
        }
        // sorted: [-500, -400, -300, -200, -100, 100, 200, 300, 400, 500], total 10
        assert_eq!(hist.value_at_percentile(0.0), -500.0); // rank 1
        assert_eq!(hist.value_at_percentile(10.0), -500.0); // rank ceil(1.0) = 1
        assert_eq!(hist.value_at_percentile(50.0), -100.0); // rank ceil(5.0) = 5
        assert_eq!(hist.value_at_percentile(90.0), 400.0); // rank ceil(9.0) = 9
        assert_eq!(hist.value_at_percentile(100.0), 500.0); // rank 10
    }

    /// With no negatives, mean/stdev/every percentile must match a plain
    /// histogram of the same precision — exercising the real combine (there is
    /// no empty-negative fast path). Percentiles are exact; mean/stdev match up
    /// to rounding. The high range puts the minimum in a wide bucket, where the
    /// Min edge convention matters.
    #[test]
    fn nonneg_matches_plain_histogram() {
        let mut rng = SmallRng::seed_from_u64(11);
        let datasets: Vec<Vec<u64>> = vec![
            vec![],                                                  // empty
            vec![0],                                                 // single zero
            vec![42],                                                // single value
            vec![7; 1000],                                           // many identical
            vec![0, 0, 0, 1, 1, 2],                                  // small, repeats, zeros
            (0..2000).map(|_| rng.random_range(0..10u64)).collect(), // small range, many
            (0..5000)
                .map(|_| rng.random_range(0..2_000_000u64))
                .collect(), // wide range
            (0..3000)
                .map(|_| rng.random_range(1_000_000..2_000_000u64))
                .collect(), // high range, min in a wide bucket
        ];
        for data in datasets {
            let mut signed = SignedHistogram::new(3).unwrap();
            let mut plain = Histogram::<u64>::new(3).unwrap();
            for &v in &data {
                signed.record(v, false).unwrap();
                plain.record(v).unwrap();
            }
            assert!(
                approx_eq(signed.mean(), plain.mean()),
                "mean, len {}: {} vs {}",
                data.len(),
                signed.mean(),
                plain.mean(),
            );
            assert!(
                approx_eq(signed.stdev(), plain.stdev()),
                "stdev, len {}: {} vs {}",
                data.len(),
                signed.stdev(),
                plain.stdev(),
            );
            for p in Percentile::iter() {
                assert_eq!(
                    signed.value_at_percentile(p.value()),
                    plain.value_at_percentile(p.value()) as f64,
                    "p{} on dataset of len {}",
                    p.value(),
                    data.len(),
                );
            }
        }
    }

    /// A symmetric set (each magnitude as +m and -m) has mean exactly zero, and
    /// percentiles antisymmetric up to one bucket width: for a magnitude m the
    /// positive side reports +highest_equivalent(m) and the negative side
    /// -lowest_equivalent(m), so p and 100-p sum to that bucket's width
    /// (highest - lowest) rather than zero.
    #[test]
    fn symmetric_distribution_is_antisymmetric() {
        let mut rng = SmallRng::seed_from_u64(13);
        let mut hist = SignedHistogram::default();
        // 2999 pairs keeps quantile*total non-integer for the tested
        // percentiles, isolating the bucket-edge residual from the integer-rank
        // gap (see value_at_percentile_at_integer_ranks).
        for _ in 0..2999 {
            let m = rng.random_range(1..2_000_000u64);
            hist.record(m, false).unwrap();
            hist.record(m, true).unwrap();
        }

        assert_eq!(hist.mean(), 0.0);

        for (lo, hi) in [
            (Percentile::P1, Percentile::P99),
            (Percentile::P5, Percentile::P95),
            (Percentile::P10, Percentile::P90),
            (Percentile::P25, Percentile::P75),
        ] {
            let low = hist.value_at_percentile(lo.value());
            let high = hist.value_at_percentile(hi.value());
            assert!(
                low < 0.0 && high > 0.0,
                "p{}={low} p{}={high}",
                lo.value(),
                hi.value()
            );
            // Read -low back from the negative store to confirm it used the
            // negative bucket; the residual is that bucket's edge gap.
            let bucket = high as u64;
            let bucket_width = high - hist.negative().lowest_equivalent(bucket) as f64;
            assert!(
                (low + high).abs() <= bucket_width + 1.0,
                "p{}/p{}: low={low} high={high} sum={} bucket_width={bucket_width}",
                lo.value(),
                hi.value(),
                low + high,
            );
        }
    }

    #[test]
    fn compute_with_errors_on_degenerate_negative_distribution() {
        let mut hist = SignedHistogram::default();
        for _ in 0..100000 {
            record(&mut hist, -1000);
        }

        let percentiles = Percentiles::compute_with_errors(&hist, 1e-6, 100000);
        let median = percentiles.get(Percentile::P50);
        assert_approx_eq!(median.value, -0.001, 0.00001);
        assert_approx_eq!(median.std_err.unwrap(), 0.000, 1e-15);
    }

    /// `add` must merge both stores; percentiles after a merge must equal those
    /// of a histogram built from all values directly (so merge is correct and
    /// order-independent), not just the means checked in value.rs.
    #[test]
    fn add_merges_both_stores() {
        let mut a = SignedHistogram::default();
        for v in [100i64, 200, -300] {
            record(&mut a, v);
        }
        let mut b = SignedHistogram::default();
        for v in [50i64, -150, -250] {
            record(&mut b, v);
        }
        a.add(&b).unwrap();
        assert_eq!(a.positive().len(), 3); // 100, 200, 50
        assert_eq!(a.negative().len(), 3); // 300, 150, 250
        assert_eq!(a.len(), 6);

        let mut direct = SignedHistogram::default();
        for v in [100i64, 200, -300, 50, -150, -250] {
            record(&mut direct, v);
        }
        for p in Percentile::iter() {
            assert_eq!(
                a.value_at_percentile(p.value()),
                direct.value_at_percentile(p.value()),
                "p{}",
                p.value(),
            );
        }
    }

    /// `clear` must empty both stores and leave a reusable, fresh histogram.
    #[test]
    fn clear_empties_both_stores() {
        let mut h = SignedHistogram::default();
        for v in [5i64, -5, 0, 7] {
            record(&mut h, v);
        }
        h.clear();
        assert!(h.is_empty());
        assert_eq!(h.len(), 0);
        assert_eq!(h.positive().len(), 0);
        assert_eq!(h.negative().len(), 0);
        assert_eq!(h.mean(), 0.0);
        assert_eq!(h.stdev(), 0.0);
        assert_eq!(h.value_at_percentile(50.0), 0.0);
        // No residual state: behaves like a fresh histogram.
        record(&mut h, 42);
        assert_eq!(h.len(), 1);
        assert_eq!(h.value_at_percentile(50.0), 42.0);
    }

    /// An empty histogram reports zero for every summary, directly (the
    /// `total == 0` / `is_empty` guards), not only via the plain-histogram
    /// comparison.
    #[test]
    fn empty_histogram_is_zero() {
        let h = SignedHistogram::default();
        assert!(h.is_empty());
        assert_eq!(h.mean(), 0.0);
        assert_eq!(h.stdev(), 0.0);
        for p in [0.0, 50.0, 100.0] {
            assert_eq!(h.value_at_percentile(p), 0.0, "p{p}");
        }
    }

    /// A single value (positive, negative, zero): mean is the value, stdev is 0,
    /// and every percentile is the value. Magnitudes are unit-resolution so the
    /// expected results are exact.
    #[test]
    fn single_value() {
        for &v in &[7i64, -7, 0] {
            let mut h = SignedHistogram::default();
            record(&mut h, v);
            assert_eq!(h.len(), 1);
            assert_eq!(h.mean(), v as f64, "mean v={v}");
            assert_eq!(h.stdev(), 0.0, "stdev v={v}");
            for p in [0.0, 50.0, 100.0] {
                assert_eq!(h.value_at_percentile(p), v as f64, "v={v} p={p}");
            }
        }
    }

    /// mean and stdev on asymmetric mixed-sign data vs an independent oracle —
    /// the only check of the cross-store combine where a sign error or wrong
    /// second moment would surface. hdr's stdev is population (÷n), as is the
    /// oracle; unit-resolution magnitudes leave only rounding.
    #[test]
    fn mean_stdev_match_oracle_on_asymmetric_signed_data() {
        let data: [i64; 7] = [-3, -3, -1, 2, 5, 5, 5];
        let mut hist = SignedHistogram::default();
        for &v in &data {
            record(&mut hist, v);
        }
        let n = data.len() as f64;
        let mean = data.iter().map(|&v| v as f64).sum::<f64>() / n;
        let variance = data.iter().map(|&v| (v as f64 - mean).powi(2)).sum::<f64>() / n;
        assert!(
            approx_eq(hist.mean(), mean),
            "mean: {} vs {mean}",
            hist.mean(),
        );
        assert!(
            approx_eq(hist.stdev(), variance.sqrt()),
            "stdev: {} vs {}",
            hist.stdev(),
            variance.sqrt(),
        );
    }
}
