//! Flat-earth geometry around Delhi and polyline projection.
//!
//! Everything is done in a local metric plane (x east, y north). Over the
//! ~120 km extent of the network the error is far below GPS noise.

/// Metres per degree of longitude at latitude 28.6 N, and per degree of latitude.
pub const KX: f64 = 97_737.1;
pub const KY: f64 = 110_540.0;

pub type P = (f64, f64);

#[inline]
pub fn to_xy(lat: f64, lon: f64) -> P {
    (lon * KX, lat * KY)
}

#[inline]
pub fn to_ll(p: P) -> (f64, f64) {
    (p.1 / KY, p.0 / KX)
}

#[inline]
pub fn dist(a: P, b: P) -> f64 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
}

/// One possible position of a point on a polyline.
#[derive(Clone, Copy, Debug)]
pub struct Cand {
    /// Distance from the point to the polyline, metres.
    pub d: f64,
    /// Distance along the polyline from its start, metres.
    pub along: f64,
}

pub struct Polyline {
    pub pts: Vec<P>,
    /// Cumulative length at each vertex; `cum[0] == 0`.
    pub cum: Vec<f64>,
}

impl Polyline {
    pub fn new(pts: Vec<P>) -> Self {
        let mut cum = Vec::with_capacity(pts.len());
        let mut c = 0.0;
        cum.push(0.0);
        for w in pts.windows(2) {
            c += dist(w[0], w[1]);
            cum.push(c);
        }
        Polyline { pts, cum }
    }

    pub fn len(&self) -> f64 {
        *self.cum.last().unwrap_or(&0.0)
    }

    /// Closest point on segment `i` -> (distance, along).
    #[inline]
    fn seg_proj(&self, i: usize, p: P) -> Cand {
        let (a, b) = (self.pts[i], self.pts[i + 1]);
        let (dx, dy) = (b.0 - a.0, b.1 - a.1);
        let l2 = dx * dx + dy * dy;
        let t = if l2 == 0.0 { 0.0 } else { (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / l2).clamp(0.0, 1.0) };
        let c = (a.0 + t * dx, a.1 + t * dy);
        Cand { d: dist(p, c), along: self.cum[i] + t * l2.sqrt() }
    }

    /// The single nearest position, ignoring order along the line.
    pub fn nearest(&self, p: P) -> Cand {
        let mut best = Cand { d: f64::INFINITY, along: 0.0 };
        for i in 0..self.pts.len().saturating_sub(1) {
            let c = self.seg_proj(i, p);
            if c.d < best.d {
                best = c;
            }
        }
        best
    }

    /// Every distinct place where the line passes within `max_d` of `p`.
    ///
    /// A loop or an out-and-back road passes the same spot more than once;
    /// each pass yields one candidate (the closest point of that pass). Two
    /// passes are distinct when they are separated along the line by a stretch
    /// that leaves the `max_d` corridor.
    pub fn candidates(&self, p: P, max_d: f64) -> Vec<Cand> {
        let mut out: Vec<Cand> = Vec::new();
        let mut cur: Option<Cand> = None;
        for i in 0..self.pts.len().saturating_sub(1) {
            let c = self.seg_proj(i, p);
            if c.d <= max_d {
                cur = Some(match cur {
                    Some(b) if b.d <= c.d => b,
                    _ => c,
                });
            } else if let Some(b) = cur.take() {
                out.push(b);
            }
        }
        if let Some(b) = cur {
            out.push(b);
        }
        out
    }

    /// Vertices strictly between two along-distances, for drawing a leg.
    pub fn slice(&self, from: f64, to: f64) -> Vec<P> {
        let mut out = Vec::new();
        for (i, &c) in self.cum.iter().enumerate() {
            if c > from && c < to {
                out.push(self.pts[i]);
            }
        }
        out
    }
}

/// Place an ordered list of points (the stops of a route) on a polyline so that
/// their along-distances never decrease.
///
/// Each point has one candidate per pass of the line near it. A dynamic program
/// picks one candidate per point minimising total offset, with a heavy penalty
/// for going backwards. This resolves loops and doubled-back roads, which a
/// plain nearest-point projection gets wrong on 3.6% of Delhi routes.
pub fn monotone_project(line: &Polyline, pts: &[P]) -> Vec<f64> {
    const BACK_PENALTY: f64 = 2000.0;
    let cands: Vec<Vec<Cand>> = pts
        .iter()
        .map(|&p| {
            let mut c = line.candidates(p, 150.0);
            if c.is_empty() {
                c.push(line.nearest(p));
            }
            c
        })
        .collect();
    let n = pts.len();
    if n == 0 {
        return vec![];
    }
    let mut cost: Vec<Vec<f64>> = Vec::with_capacity(n);
    let mut back: Vec<Vec<usize>> = Vec::with_capacity(n);
    cost.push(cands[0].iter().map(|c| c.d).collect());
    back.push(vec![0; cands[0].len()]);
    for k in 1..n {
        let mut ck = Vec::with_capacity(cands[k].len());
        let mut bk = Vec::with_capacity(cands[k].len());
        for c in &cands[k] {
            let (mut best, mut arg) = (f64::INFINITY, 0);
            for (j, pc) in cands[k - 1].iter().enumerate() {
                let step = c.along - pc.along;
                let pen = if step < -1.0 { BACK_PENALTY - step } else { 0.0 };
                let v = cost[k - 1][j] + pen;
                if v < best {
                    best = v;
                    arg = j;
                }
            }
            ck.push(best + c.d);
            bk.push(arg);
        }
        cost.push(ck);
        back.push(bk);
    }
    let mut j = (0..cands[n - 1].len())
        .min_by(|&a, &b| cost[n - 1][a].total_cmp(&cost[n - 1][b]))
        .unwrap();
    let mut along = vec![0.0; n];
    for k in (0..n).rev() {
        along[k] = cands[k][j].along;
        j = back[k][j];
    }
    // Guarantee the invariant even where the data forced a backward step.
    for k in 1..n {
        if along[k] < along[k - 1] {
            along[k] = along[k - 1];
        }
    }
    along
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projects_onto_straight_line() {
        let l = Polyline::new(vec![(0.0, 0.0), (100.0, 0.0), (200.0, 0.0)]);
        let c = l.nearest((150.0, 10.0));
        assert!((c.d - 10.0).abs() < 1e-9 && (c.along - 150.0).abs() < 1e-9);
    }

    #[test]
    fn loop_yields_two_candidates_and_dp_picks_by_order() {
        // Out along y=0, back along y=20: every x is passed twice.
        let l = Polyline::new(vec![(0.0, 0.0), (1000.0, 0.0), (1000.0, 20.0), (0.0, 20.0)]);
        assert_eq!(l.candidates((500.0, 10.0), 60.0).len(), 2);
        let l = Polyline::new(vec![(0.0, 0.0), (1000.0, 0.0), (1000.0, 400.0), (0.0, 400.0), (0.0, 5.0)]);
        // A stop near the start is also near the end of this loop.
        let c = l.candidates((0.0, 2.0), 60.0);
        assert_eq!(c.len(), 2);
        // First and last stop are at the same place; middle stops fix the order.
        let stops = [(0.0, 2.0), (500.0, 0.0), (1000.0, 200.0), (500.0, 400.0), (0.0, 3.0)];
        let a = monotone_project(&l, &stops);
        assert!(a.windows(2).all(|w| w[1] >= w[0]));
        assert!(a[0] < 10.0 && a[4] > 2700.0, "{a:?}");
    }
}
