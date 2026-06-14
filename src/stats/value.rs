use crate::stats::histogram::SerializableHistogram;
use crate::stats::percentiles::Percentiles;
use crate::stats::signed_histogram::SignedHistogram;
use crate::stats::timeseries::TimeSeriesStats;
use crate::stats::Mean;
use serde::{Deserialize, Serialize};

/// A dimensionless metric value.
/// Stored with 1e-6 resolution; zero is stored exactly and negative values
/// are stored as magnitudes in a separate histogram.
/// Values must be finite — the recording API (`record_metric`) rejects
/// everything else before it gets here.
#[derive(Copy, Clone, Debug)]
pub struct MetricValue(pub f64);

impl MetricValue {
    /// Histogram units per metric point: values are stored in 1e-6 units,
    /// giving 6 significant decimal digits.
    const STORED_PER_POINT: f64 = 1e6;
    /// Scale from integer histogram units back to reported numbers.
    const DISPLAY_SCALE: f64 = 1.0 / Self::STORED_PER_POINT;

    /// Magnitude in integer histogram units, exactly where possible.
    fn magnitude_stored(self) -> u64 {
        debug_assert!(self.0.is_finite());
        (self.0.abs() * Self::STORED_PER_POINT)
            .round()
            .min(u64::MAX as f64) as u64
    }

    /// Signed value in histogram units for time-series statistics,
    /// which are scale-invariant.
    fn stored_f64(self) -> f64 {
        self.0 * Self::STORED_PER_POINT
    }
}

/// Captures the mean and percentiles of a signed distribution,
/// with uncertainty estimates.
/// Negative values live in a separate histogram holding their magnitudes.
#[derive(Serialize, Deserialize, Debug)]
pub struct ValueDistribution {
    pub mean: Mean,
    pub percentiles: Percentiles,
    /// Non-negative recorded values, zero included.
    pub histogram: SerializableHistogram,
    /// Magnitudes of negative recorded values.
    /// Omitted from reports while empty, so reports of metrics that never
    /// went negative keep their shape; reports without it load as empty.
    #[serde(default, skip_serializing_if = "SerializableHistogram::is_empty")]
    pub negative_histogram: SerializableHistogram,
}

/// Builds ValueDistribution from a stream of measured values.
#[derive(Clone, Debug, Default)]
pub struct ValueDistributionRecorder {
    histogram: SignedHistogram,
    ess_estimator: TimeSeriesStats,
}

impl ValueDistributionRecorder {
    pub fn record(&mut self, value: MetricValue) {
        let magnitude = value.magnitude_stored();
        // A magnitude that rounds to zero is zero at our resolution, so it
        // belongs in the non-negative store regardless of its sign.
        let negative = value.0 < 0.0 && magnitude > 0;
        self.histogram.record(magnitude, negative).unwrap();
        self.ess_estimator.record(value.stored_f64(), 1.0);
    }

    pub fn add(&mut self, other: &ValueDistributionRecorder) {
        self.histogram.add(&other.histogram).unwrap();
        self.ess_estimator.add(&other.ess_estimator);
    }

    pub fn clear(&mut self) {
        self.histogram.clear();
        self.ess_estimator.clear();
    }

    pub fn distribution(&self) -> ValueDistribution {
        ValueDistribution {
            mean: self.mean(1),
            percentiles: Percentiles::compute(&self.histogram, MetricValue::DISPLAY_SCALE),
            histogram: SerializableHistogram(self.histogram.positive().clone()),
            negative_histogram: SerializableHistogram(self.histogram.negative().clone()),
        }
    }

    pub fn distribution_with_errors(&self) -> ValueDistribution {
        let ess = self.ess_estimator.effective_sample_size();
        ValueDistribution {
            mean: self.mean(ess),
            percentiles: Percentiles::compute_with_errors(
                &self.histogram,
                MetricValue::DISPLAY_SCALE,
                ess,
            ),
            histogram: SerializableHistogram(self.histogram.positive().clone()),
            negative_histogram: SerializableHistogram(self.histogram.negative().clone()),
        }
    }

    fn mean(&self, effective_n: u64) -> Mean {
        let scale = MetricValue::DISPLAY_SCALE;
        Mean {
            n: effective_n,
            value: self.histogram.mean() * scale,
            std_err: if effective_n > 1 {
                Some(self.histogram.stdev() * scale / (effective_n as f64 - 1.0).sqrt())
            } else {
                None
            },
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::stats::percentiles::Percentile;

    #[test]
    fn stores_zero_exactly() {
        let mut recorder = ValueDistributionRecorder::default();
        recorder.record(MetricValue(0.0));
        recorder.record(MetricValue(0.0));
        let dist = recorder.distribution();
        assert_eq!(dist.mean.value, 0.0);
        assert_eq!(dist.percentiles.get(Percentile::P50).value, 0.0);
        assert_eq!(dist.percentiles.get(Percentile::Max).value, 0.0);
    }

    #[test]
    fn records_and_computes_distribution() {
        let mut recorder = ValueDistributionRecorder::default();
        recorder.record(MetricValue(0.5));
        recorder.record(MetricValue(1.0));
        let dist = recorder.distribution();
        assert!((dist.mean.value - 0.75).abs() < 0.001);
    }

    #[test]
    fn merges_recorders() {
        let mut a = ValueDistributionRecorder::default();
        let mut b = ValueDistributionRecorder::default();
        a.record(MetricValue(0.2));
        b.record(MetricValue(0.8));
        a.add(&b);
        let dist = a.distribution();
        assert!((dist.mean.value - 0.5).abs() < 0.001);
    }

    #[test]
    fn merges_recorders_with_negative_values() {
        let mut a = ValueDistributionRecorder::default();
        let mut b = ValueDistributionRecorder::default();
        a.record(MetricValue(-0.2));
        a.record(MetricValue(0.4));
        b.record(MetricValue(-0.6));
        a.add(&b);
        let dist = a.distribution();
        assert!((dist.mean.value + 0.1333).abs() < 0.001);
        assert!(!dist.negative_histogram.is_empty());
    }

    #[test]
    fn records_negative_values() {
        let mut recorder = ValueDistributionRecorder::default();
        for v in [-2.0, -1.0, 0.0, 1.0, 2.0] {
            recorder.record(MetricValue(v));
        }
        let dist = recorder.distribution();
        assert!(dist.mean.value.abs() < 1e-6);
        assert!((dist.percentiles.get(Percentile::Min).value + 2.0).abs() < 0.01);
        assert!(dist.percentiles.get(Percentile::P50).value.abs() < 1e-6);
        assert!((dist.percentiles.get(Percentile::Max).value - 2.0).abs() < 0.01);
        assert!(!dist.negative_histogram.is_empty());
    }

    #[test]
    fn records_all_negative_distribution() {
        let mut recorder = ValueDistributionRecorder::default();
        for v in [-3.0, -2.0, -1.0] {
            recorder.record(MetricValue(v));
        }
        let dist = recorder.distribution();
        assert!((dist.mean.value + 2.0).abs() < 0.01);
        assert!((dist.percentiles.get(Percentile::Min).value + 3.0).abs() < 0.01);
        assert!((dist.percentiles.get(Percentile::P50).value + 2.0).abs() < 0.01);
        assert!((dist.percentiles.get(Percentile::Max).value + 1.0).abs() < 0.01);
    }

    #[test]
    fn survives_serde_roundtrip() {
        let mut recorder = ValueDistributionRecorder::default();
        recorder.record(MetricValue(0.9933));
        let json = serde_json::to_string(&recorder.distribution()).unwrap();
        let parsed: ValueDistribution = serde_json::from_str(&json).unwrap();
        assert!((parsed.mean.value - 0.9933).abs() < 0.001);
    }

    #[test]
    fn positive_only_omits_negative_histogram_in_json() {
        let mut recorder = ValueDistributionRecorder::default();
        recorder.record(MetricValue(0.5));
        let json = serde_json::to_string(&recorder.distribution()).unwrap();
        assert!(!json.contains("negative_histogram"));
        let parsed: ValueDistribution = serde_json::from_str(&json).unwrap();
        assert!(parsed.negative_histogram.is_empty());
    }

    #[test]
    fn negative_values_survive_serde_roundtrip() {
        let mut recorder = ValueDistributionRecorder::default();
        recorder.record(MetricValue(-0.25));
        recorder.record(MetricValue(0.75));
        let dist = recorder.distribution();
        let json = serde_json::to_string(&dist).unwrap();
        let parsed: ValueDistribution = serde_json::from_str(&json).unwrap();
        assert!((parsed.mean.value - 0.25).abs() < 0.001);
        assert!(!parsed.negative_histogram.is_empty());
        for p in [Percentile::Min, Percentile::P50, Percentile::Max] {
            assert!((parsed.percentiles.get(p).value - dist.percentiles.get(p).value).abs() < 1e-9);
        }
    }

    /// The zero-routing invariant: a negative value whose magnitude rounds to 0
    /// is zero at our 1e-6 resolution, so it goes to the non-negative store, not
    /// the negative one. Pins the exact boundary against the resolution floor.
    #[test]
    fn tiny_negative_routes_to_positive_store() {
        // |magnitude| = round(-1e-9 * 1e6) = round(-0.001) = 0 -> positive store.
        let mut r = ValueDistributionRecorder::default();
        r.record(MetricValue(-1e-9));
        assert!(r.histogram.negative().is_empty());
        assert_eq!(r.histogram.positive().len(), 1);
        assert_eq!(r.distribution().mean.value, 0.0);

        // |magnitude| = round(-1e-6 * 1e6) = round(-1.0) = 1 -> negative store.
        let mut r2 = ValueDistributionRecorder::default();
        r2.record(MetricValue(-1e-6));
        assert_eq!(r2.histogram.negative().len(), 1);
        assert!(r2.histogram.positive().is_empty());
    }
}
