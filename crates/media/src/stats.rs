//! Rolling measurement windows and percentiles for periodic reports.

use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
pub struct Series(Arc<Mutex<Vec<u64>>>);

impl Series {
    pub fn push(&self, v: u64) {
        self.0.lock().unwrap().push(v);
    }

    /// Takes all values collected since the last call.
    pub fn take(&self) -> Summary {
        let mut v = std::mem::take(&mut *self.0.lock().unwrap());
        v.sort_unstable();
        Summary(v)
    }
}

pub struct Summary(Vec<u64>);

impl Summary {
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Percentile in milliseconds, for values stored in nanoseconds.
    pub fn ms(&self, p: f64) -> f64 {
        if self.0.is_empty() {
            return f64::NAN;
        }
        let i = ((self.0.len() - 1) as f64 * p).round() as usize;
        self.0[i] as f64 / 1e6
    }

    /// Percentile above the smallest value, in milliseconds, for nanosecond values.
    pub fn spread_ms(&self, p: f64) -> f64 {
        if self.0.is_empty() {
            return f64::NAN;
        }
        let i = ((self.0.len() - 1) as f64 * p).round() as usize;
        (self.0[i] - self.0[0]) as f64 / 1e6
    }

    pub fn count_over(&self, ns: u64) -> usize {
        self.0.iter().filter(|&&x| x > ns).count()
    }
}
