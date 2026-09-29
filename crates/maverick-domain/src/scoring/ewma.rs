//! The EWMA fold with Welford variance (spec §6.4, reference Part 3 §1.8).

/// The default EWMA weight (`MAVERICK_SCORING_ALPHA`).
pub const ALPHA: f64 = 0.3;
/// The default observation count at which a cell is measured
/// (`MAVERICK_SCORING_MIN_NIGHTS`).
pub const MIN_NIGHTS: u64 = 3;

/// Round to IEEE-754 binary32 and widen back: a Postgres `real` column's round trip.
pub fn f32_round(x: f64) -> f64 {
    f64::from(x as f32)
}

/// One (cell, dimension) series: the EWMA seeded by the first observation, and the
/// Welford running variance of the observations.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stream {
    alpha: f64,
    n: u64,
    ewma: f64,
    mean: f64,
    m2: f64,
}

impl Stream {
    pub fn new(alpha: f64) -> Self {
        Stream {
            alpha,
            n: 0,
            ewma: 0.0,
            mean: 0.0,
            m2: 0.0,
        }
    }

    /// Folds one observation: `S_0 = x_0`, then `S_t = α·x_t + (1-α)·S_{t-1}`; Welford
    /// updates the mean and `m2`.
    pub fn fold(&mut self, x: f64) {
        self.ewma = if self.n == 0 {
            x
        } else {
            self.alpha * x + (1.0 - self.alpha) * self.ewma
        };
        self.n += 1;
        let delta = x - self.mean;
        self.mean += delta / self.n as f64;
        self.m2 += delta * (x - self.mean);
    }

    /// The smoothed value (0 before any fold).
    pub fn ewma(&self) -> f64 {
        self.ewma
    }

    /// The population variance of the observations (0 before any fold). Go's
    /// `math.Max(m2/n, 0)`: NaN stays NaN and `-0` becomes `0`.
    pub fn variance(&self) -> f64 {
        if self.n == 0 {
            return 0.0;
        }
        let variance = self.m2 / self.n as f64;
        if variance.is_nan() || variance > 0.0 {
            variance
        } else {
            0.0
        }
    }

    /// The number of observations folded.
    pub fn n(&self) -> u64 {
        self.n
    }

    /// `n >= min_nights`.
    pub fn measured(&self, min_nights: u64) -> bool {
        self.n >= min_nights
    }

    /// `Resume(alpha, ewmas)`: rebuilds a stream from a stored smoothed series (ascending
    /// by date) by inverting the fold, `x_0 = S_0` and
    /// `x_t = (S_t - (1-α)·S_{t-1}) / α`, then folding each recovered observation. The
    /// variance is recomputed from the recovered observations.
    pub fn resume(alpha: f64, ewmas: &[f64]) -> Self {
        let mut stream = Stream::new(alpha);
        let mut previous = None;
        for &smoothed in ewmas {
            let x = match previous {
                None => smoothed,
                Some(prev) => (smoothed - (1.0 - alpha) * prev) / alpha,
            };
            stream.fold(x);
            previous = Some(smoothed);
        }
        stream
    }
}

/// One stored `aggregates` row's values after a fold, as the `real` columns hold them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StoredAggregate {
    /// `f32(ewma)`, widened.
    pub ewma: f64,
    /// `f32(variance)`, widened.
    pub variance: f64,
    pub n: u64,
    pub measured: bool,
}

/// The pure core of `FoldObservations` for one (cell, dimension): resume from the
/// stored (float32-widened) EWMAs of the earlier nights, fold this night's observation,
/// and round the results to float32 as the `real` columns store them.
pub fn fold_observation(
    alpha: f64,
    stored_ewmas: &[f64],
    observation: f64,
    min_nights: u64,
) -> StoredAggregate {
    let mut stream = Stream::resume(alpha, stored_ewmas);
    stream.fold(observation);
    StoredAggregate {
        ewma: f32_round(stream.ewma()),
        variance: f32_round(stream.variance()),
        n: stream.n(),
        measured: stream.measured(min_nights),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variance_follows_go_max() {
        let mut stream = Stream::new(ALPHA);
        stream.fold(f64::NAN);
        assert!(stream.variance().is_nan());
        let negative = Stream {
            alpha: ALPHA,
            n: 1,
            ewma: 0.0,
            mean: 0.0,
            m2: -0.0,
        };
        assert_eq!(negative.variance().to_bits(), 0.0f64.to_bits());
        let below = Stream {
            m2: -1.0,
            ..negative
        };
        assert_eq!(below.variance(), 0.0);
    }

    #[test]
    fn f32_round_trips_like_real() {
        assert_eq!(f32_round(0.8), 0.800000011920929);
        assert_eq!(f32_round(1450.0), 1450.0);
    }

    #[test]
    fn measured_boundary() {
        let mut stream = Stream::new(ALPHA);
        stream.fold(1.0);
        stream.fold(1.0);
        assert!(!stream.measured(MIN_NIGHTS));
        assert!(stream.measured(2));
    }
}
