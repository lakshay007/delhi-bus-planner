//! Combining live and historical data.
//!
//! History says what a segment usually takes at this time. The last hour of
//! bus movements says how today differs. For each segment we compute a live
//! ratio — recent observed time divided by the usual time — and apply it as
//! a correction that fades the further ahead we look:
//!
//!   seconds = usual(segment, time) x ratio(segment) ^ fade(lead)
//!   fade(lead) = exp(-lead / 6 h)
//!
//! `ratio(segment)` is an age-weighted mean of observed / usual (the same
//! statistic the model is fitted with), and itself a blend, weighted by how
//! much live evidence there is: the segment's own recent traversals and its
//! neighbours', pulled toward the city-wide live ratio, which is pulled
//! toward 1. So:
//!
//!   - plenty of buses just went through  -> follows what they did
//!   - no bus on this stretch lately      -> follows the city-wide mood
//!                                           (a wet evening slows everything)
//!   - no live data at all                -> ratio 1, pure history
//!   - trip leaves many hours from now    -> fade shrinks toward pure history
//!
//! The fade is slow because replay on a month of archive showed it should
//! be: a stretch that is slower than usual now is mostly still slower than
//! usual hours later.

use crate::gtfs::Network;
use crate::link::{Traversal, VehicleState};
use crate::raptor::SegTimes;
use crate::clock::Local;
use std::collections::HashMap;
use std::sync::Arc;

/// Traversals older than this are never used; callers load this much history.
pub const WINDOW_S: i64 = 60 * 60;

/// Settings of the live correction. The defaults were chosen by replay on the
/// October 2026 archive; `busplan tune --set live` compares
/// alternatives.
#[derive(Clone, Debug)]
pub struct LiveParams {
    /// Traversals older than this are ignored (at most `WINDOW_S`).
    pub window_s: f32,
    /// A traversal's weight falls by e every this many seconds of age.
    pub tau_age_s: f32,
    /// How long a deviation from normal persists: the correction falls by e
    /// for every this many seconds between now and the bus reaching the
    /// segment. Six hours: replay on the archive showed that what makes a
    /// stretch slow today mostly lasts the day.
    pub tau_lead_s: f32,
    /// Pseudo-observations pulling a segment's ratio toward the city-wide one.
    pub k_seg: f32,
    /// Pseudo-observations pulling the city-wide ratio toward 1.
    pub k_city: f32,
    /// Weight of a neighbouring segment's traversal relative to the segment's own.
    pub neighbour_w: f32,
    /// One traversal may claim at most this deviation from normal.
    pub cap: (f32, f32),
}

impl Default for LiveParams {
    fn default() -> Self {
        LiveParams { window_s: 3600.0, tau_age_s: 900.0, tau_lead_s: 21_600.0, k_seg: 3.0, k_city: 50.0, neighbour_w: 0.5, cap: (0.33, 3.0) }
    }
}

/// Live deviation from the usual travel time, per segment, as of `now`.
pub struct LiveRatios {
    /// ln(ratio) per segment, already shrunk.
    ln_r: Vec<f32>,
    /// Effective number of recent traversals on the segment and its neighbours.
    n_eff: Vec<f32>,
    pub ln_city: f32,
    pub now: i64,
    /// Traversals inside the window.
    pub n_obs: usize,
    /// Segments with at least one own or neighbouring traversal.
    pub n_segs: usize,
    p: LiveParams,
}

impl LiveRatios {
    /// `trav` may contain anything; only traversals that finished within the
    /// window before `now` are used.
    pub fn compute(net: &Network, base: &dyn SegTimes, trav: &[Traversal], now: i64) -> LiveRatios {
        Self::compute_with(net, base, trav, now, &LiveParams::default())
    }

    pub fn compute_with(net: &Network, base: &dyn SegTimes, trav: &[Traversal], now: i64, p: &LiveParams) -> LiveRatios {
        let items = trav.iter().filter_map(|t| {
            let age = now as f64 - (t.t_enter + t.dur as f64);
            if age < 0.0 || age > p.window_s as f64 || net.segments[t.seg as usize].len_m < 30.0 {
                return None;
            }
            let at = Local::from_epoch(t.t_enter as i64);
            Some((t.seg, age as f32, t.dur / base.secs(net, t.seg, at.day_type(), at.tod)))
        });
        Self::from_ratios(net, items, now, p)
    }

    /// From (segment, seconds since the traversal finished, observed / usual).
    pub fn from_ratios(net: &Network, items: impl Iterator<Item = (u32, f32, f32)>, now: i64, p: &LiveParams) -> LiveRatios {
        let ns = net.segments.len();
        let mut sum = vec![0f32; ns];
        let mut wsum = vec![0f32; ns];
        let (mut csum, mut cw, mut n_obs) = (0f32, 0f32, 0usize);
        for (seg, age, ratio) in items {
            if age < 0.0 || age > p.window_s {
                continue;
            }
            // Arithmetic mean of the capped ratios, the same statistic the
            // historical model is fitted with. A mean of logs (geometric mean)
            // sits below 1 on an ordinary day because travel times are
            // right-skewed, which made every live estimate a little too short.
            let r = ratio.clamp(p.cap.0, p.cap.1);
            let w = (-age / p.tau_age_s).exp();
            sum[seg as usize] += w * r;
            wsum[seg as usize] += w;
            csum += w * r;
            cw += w;
            n_obs += 1;
        }
        let city = (csum + p.k_city) / (cw + p.k_city);
        let ln_city = city.ln();
        let mut ln_r = vec![ln_city; ns];
        let mut n_eff = vec![0f32; ns];
        let mut n_segs = 0;
        for s in 0..ns {
            let (mut a, mut w) = (sum[s], wsum[s]);
            for &o in &net.seg_neighbours[s] {
                a += p.neighbour_w * sum[o as usize];
                w += p.neighbour_w * wsum[o as usize];
            }
            if w > 0.0 {
                n_segs += 1;
                ln_r[s] = ((a + p.k_seg * city) / (w + p.k_seg)).ln();
                n_eff[s] = w;
            }
        }
        LiveRatios { ln_r, n_eff, ln_city, now, n_obs, n_segs, p: p.clone() }
    }

    /// The city-wide live ratio: 1.2 means buses are running 20% slower than usual.
    pub fn city_ratio(&self) -> f32 {
        self.ln_city.exp()
    }

    /// Seconds from now until the bus is on a segment it reaches at `tod`.
    fn lead(&self, tod: u32) -> f32 {
        let now_tod = Local::from_epoch(self.now).tod as i64;
        let mut lead = (tod as i64 - now_tod).rem_euclid(86_400);
        if lead > 43_200 {
            lead -= 86_400; // slightly in the past: treat as now
        }
        lead.max(0) as f32
    }

    /// How much of the live ratio still applies to a segment reached at `tod`.
    fn fade(&self, tod: u32) -> f32 {
        (-self.lead(tod) / self.p.tau_lead_s).exp()
    }
}

/// Historical times corrected by the live ratios.
/// How much service each route is really giving right now, as observed: how
/// many buses passed each stop in the last hour, and how many are standing at
/// the route's first stop. Facts to set beside a timetabled departure.
#[derive(Default)]
pub struct Service {
    /// Per route: buses seen crossing each stop-to-stop hop in the last hour.
    hops: HashMap<u32, Vec<u16>>,
    /// Per route: buses standing at its first stop, however long they have been there.
    standing: HashMap<u32, u32>,
}

/// A hop's crossing is sometimes lost to a GPS gap, so a stop's count is the
/// most seen over this many hops from it.
const SERVICE_HOPS: usize = 4;

impl Service {
    pub fn build(net: &Network, trav: &[Traversal], buses: &[VehicleState], now: i64) -> Service {
        let mut s = Service::default();
        for t in trav {
            let age = now as f64 - t.t_enter;
            if (0.0..=WINDOW_S as f64).contains(&age) {
                let v = s.hops.entry(t.route).or_insert_with(|| vec![0; net.routes[t.route as usize].segs.len()]);
                if let Some(c) = v.get_mut(t.k as usize) {
                    *c += 1;
                }
            }
        }
        for b in buses.iter().filter(|b| b.waiting_s.is_some()) {
            *s.standing.entry(b.route).or_default() += 1;
        }
        s
    }

    /// Buses of `route` seen leaving stop position `k` in the last hour.
    pub fn seen(&self, route: u32, k: usize) -> u32 {
        self.hops.get(&route).map_or(0, |v| v.iter().skip(k).take(SERVICE_HOPS).copied().max().unwrap_or(0) as u32)
    }

    /// Timetabled departures of `route` from stop position `k` in the hour
    /// before `now_tod`, counting every trip of the GTFS route that calls
    /// there (its short-turn trips too).
    pub fn timetabled(net: &Network, route: u32, k: usize, now_tod: u32) -> u32 {
        let stop = net.routes[route as usize].stops.get(k).copied();
        net.family(route)
            .filter(|&e| e == route || (stop.is_some() && net.routes[e as usize].stops.get(k).copied() == stop))
            .map(|e| {
                let r = &net.routes[e as usize];
                let off = r.sched.get(k).copied().unwrap_or(0);
                r.starts.iter().filter(|&&st| st + off <= now_tod && st + off + WINDOW_S as u32 > now_tod).count() as u32
            })
            .sum()
    }

    /// Buses of `route` standing at its first stop now.
    pub fn standing(&self, route: u32) -> u32 {
        self.standing.get(&route).copied().unwrap_or(0)
    }
}

pub struct Blended {
    pub base: Arc<dyn SegTimes>,
    pub live: Arc<LiveRatios>,
}

impl SegTimes for Blended {
    fn secs(&self, net: &Network, seg: u32, day_type: usize, tod: u32) -> f32 {
        let usual = self.base.secs(net, seg, day_type, tod);
        usual * (self.live.fade(tod) * self.live.ln_r[seg as usize]).exp()
    }

    fn quiet_secs(&self, net: &Network, seg: u32) -> f32 {
        self.base.quiet_secs(net, seg)
    }

    fn level(&self, seg: u32, day_type: usize, tod: u32) -> u8 {
        self.base.level(seg, day_type, tod)
    }

    fn live_weight(&self, seg: u32, tod: u32) -> f32 {
        let n = self.live.n_eff[seg as usize];
        self.live.fade(tod) * n / (n + self.live.p.k_seg)
    }

    fn name(&self) -> &'static str {
        if self.base.name() == "model" { "model+live" } else { "timetable+live" }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::Polyline;
    use crate::gtfs::{Route, Segment, Stop};
    use crate::raptor::Scheduled;
    use std::collections::HashMap;

    /// Four 500 m segments in a row; the timetable says 100 s each.
    fn toy() -> Network {
        let stops: Vec<Stop> =
            (0..5).map(|i| Stop { id: format!("s{i}"), name: String::new(), lat: 0.0, lon: 0.0, xy: (i as f64 * 500.0, 0.0) }).collect();
        let segments: Vec<Segment> = (0..4).map(|i| Segment { a: i, b: i + 1, len_m: 500.0, sched_s: 100.0, n_routes: 1 }).collect();
        let route = Route {
            id: "r".into(),
            name: "1UP".into(),
            desc: String::new(),
            agency: String::new(),
            stops: (0..5).collect(),
            sched: vec![0, 100, 200, 300, 400],
            starts: vec![0],
            main: 0,
            shape: Polyline::new(vec![(0.0, 0.0), (2000.0, 0.0)]),
            along: (0..5).map(|i| i as f64 * 500.0).collect(),
            segs: vec![0, 1, 2, 3],
        };
        Network {
            stops,
            routes: vec![route],
            segments,
            stop_index: HashMap::new(),
            route_index: HashMap::new(),
            seg_index: HashMap::new(),
            stop_routes: vec![],
            footpaths: vec![],
            seg_neighbours: vec![vec![1], vec![0, 2], vec![1, 3], vec![2]],
            siblings: vec![],
            variants: vec![],
        }
    }

    fn trav(seg: u32, t_enter: i64, dur: f32) -> Traversal {
        Traversal { seg, route: 0, k: seg as u16, t_enter: t_enter as f64, dur, gap: 20.0 }
    }

    const NOW: i64 = 1_794_600_000;

    fn blended(tr: &[Traversal]) -> Blended {
        let net = toy();
        let base: Arc<dyn SegTimes> = Arc::new(Scheduled);
        let live = Arc::new(LiveRatios::compute(&net, base.as_ref(), tr, NOW));
        Blended { base, live }
    }

    #[test]
    fn no_live_data_is_pure_history() {
        let (net, b) = (toy(), blended(&[]));
        let tod = Local::from_epoch(NOW).tod;
        assert_eq!(b.secs(&net, 0, 0, tod), 100.0);
        assert_eq!(b.live_weight(0, tod), 0.0);
    }

    #[test]
    fn recent_slow_buses_slow_the_estimate_and_the_effect_fades() {
        let net = toy();
        // Twenty buses just took 200 s on segment 0 (twice the usual).
        let tr: Vec<Traversal> = (0..20).map(|i| trav(0, NOW - 260 - i * 20, 200.0)).collect();
        let b = blended(&tr);
        let tod = Local::from_epoch(NOW).tod;
        let now = b.secs(&net, 0, 0, tod);
        assert!(now > 165.0 && now <= 200.0, "leaving now: {now}");
        // Half an hour ahead most of the correction still applies...
        let soon = b.secs(&net, 0, 0, tod + 1800);
        assert!(soon < now && soon > now - 12.0, "in 30 min: {soon}");
        // ...six hours ahead about a third of it (in log terms)...
        let later = b.secs(&net, 0, 0, (tod + 6 * 3600) % 86_400);
        assert!(later > 115.0 && later < 135.0, "in 6 h: {later}");
        // ...and it only ever shrinks.
        assert!(b.secs(&net, 0, 0, (tod + 11 * 3600) % 86_400) < later);
        // The neighbour inherits part of the slowdown; a far segment only the city-wide share.
        let (near, far) = (b.secs(&net, 1, 0, tod), b.secs(&net, 3, 0, tod));
        assert!(near > far && far >= 100.0 && near < now, "near {near}, far {far}");
        assert!(b.live_weight(0, tod) > b.live_weight(1, tod) && b.live_weight(3, tod) == 0.0);
    }

    #[test]
    fn stale_and_future_traversals_are_ignored() {
        let net = toy();
        let b = blended(&[trav(0, NOW - 3 * 3600, 300.0), trav(0, NOW + 600, 300.0)]);
        assert_eq!(b.live.n_obs, 0);
        assert_eq!(b.secs(&net, 0, 0, Local::from_epoch(NOW).tod), 100.0);
    }

    #[test]
    fn one_wild_bus_moves_the_estimate_only_a_little() {
        let net = toy();
        let b = blended(&[trav(0, NOW - 1000, 900.0)]);
        let v = b.secs(&net, 0, 0, Local::from_epoch(NOW).tod);
        // The bus took 9x the usual time. The cap counts it as 3x and the
        // pull toward the city ratio halves that again: about 1.5x, not 9x.
        assert!(v > 100.0 && v < 160.0, "{v}");
    }
}
