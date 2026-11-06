//! Marks a viewer whose connection cannot keep up. Only a mark: nothing about the stream
//! changes for this viewer or anyone else (decision D1, isolation requirement).

use crate::host::ViewerQuality;
use std::collections::HashMap;

/// Loss share in the last receiver report above which a viewer is marked.
pub const LOSS_LIMIT: f64 = 0.02;
/// Round-trip time growth over the viewer's best one above which packets are queueing
/// somewhere on the way: the link is slower than the stream.
pub const QUEUE_LIMIT_MS: f64 = 100.0;

#[derive(Debug, Clone, PartialEq)]
pub enum Problem {
    Loss { pct: f64 },
    /// Round-trip time this much above the best seen for this viewer.
    Queue { ms: f64 },
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Problem::Loss { pct } => write!(f, "loss {pct:.1}%"),
            Problem::Queue { ms } => write!(f, "queue +{ms:.0} ms"),
        }
    }
}

/// Remembers each viewer's best round-trip time, the baseline for queue growth.
#[derive(Default)]
pub struct Marker {
    best_rtt_ms: HashMap<u64, f64>,
}

impl Marker {
    pub fn assess(&mut self, q: &ViewerQuality) -> Vec<Problem> {
        let mut out = Vec::new();
        if q.fraction_lost > LOSS_LIMIT {
            out.push(Problem::Loss { pct: q.fraction_lost * 100.0 });
        }
        // 0 means no round-trip measurement yet.
        if q.rtt_ms > 0.0 {
            let best = self.best_rtt_ms.entry(q.id).or_insert(q.rtt_ms);
            *best = best.min(q.rtt_ms);
            let growth = q.rtt_ms - *best;
            if growth > QUEUE_LIMIT_MS {
                out.push(Problem::Queue { ms: growth });
            }
        }
        out
    }

    pub fn forget(&mut self, id: u64) {
        self.best_rtt_ms.remove(&id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(id: u64, lost: f64, rtt: f64) -> ViewerQuality {
        ViewerQuality { id, fraction_lost: lost, rtt_ms: rtt, ..Default::default() }
    }

    #[test]
    fn healthy_viewer_is_not_marked() {
        let mut m = Marker::default();
        for rtt in [40.0, 45.0, 60.0, 120.0] {
            assert!(m.assess(&q(1, 0.001, rtt)).is_empty());
        }
    }

    #[test]
    fn queue_growth_is_marked_per_viewer() {
        let mut m = Marker::default();
        m.assess(&q(1, 0.0, 30.0));
        m.assess(&q(2, 0.0, 200.0));
        assert_eq!(m.assess(&q(1, 0.0, 400.0)), vec![Problem::Queue { ms: 370.0 }]);
        // Another viewer's high but steady round trip is its normal, not a problem.
        assert!(m.assess(&q(2, 0.0, 220.0)).is_empty());
    }

    #[test]
    fn loss_is_marked_and_missing_rtt_ignored() {
        let mut m = Marker::default();
        assert_eq!(m.assess(&q(1, 0.05, 0.0)), vec![Problem::Loss { pct: 5.0 }]);
    }
}
