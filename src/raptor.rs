//! RAPTOR over trips whose stop times come from a travel-time provider.
//!
//! The GTFS timetable only contributes each trip's start time. Every later
//! arrival is `previous arrival + provider.secs(segment, time the bus is there)`,
//! which is what makes the planner's answers depend on time of day.

use crate::clock::Local;
use crate::gtfs::Network;
use crate::link::VehicleState;
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};

/// Anything that can say how long a bus takes over one stop-to-stop segment.
pub trait SegTimes: Sync + Send {
    /// Seconds to traverse `seg` when entering it at `tod` seconds after
    /// midnight on a day of kind `day_type` (clock::Local::day_type).
    fn secs(&self, net: &Network, seg: u32, day_type: usize, tod: u32) -> f32;

    /// Seconds at the quietest time of day — the "no traffic" reference.
    fn quiet_secs(&self, net: &Network, seg: u32) -> f32 {
        self.secs(net, seg, 0, 5 * 3600)
    }

    /// Which rung of the fallback ladder produced
    /// the estimate: 1 = this segment at this time ... 7 = timetable.
    fn level(&self, _seg: u32, _day_type: usize, _tod: u32) -> u8 {
        7
    }

    /// How much of this estimate rests on buses seen in the last hour (0 to 1).
    fn live_weight(&self, _seg: u32, _tod: u32) -> f32 {
        0.0
    }

    fn name(&self) -> &'static str;
}

/// The published timetable: 18 km/h everywhere, all day. The baseline.
pub struct Scheduled;

impl SegTimes for Scheduled {
    fn secs(&self, net: &Network, seg: u32, _: usize, _: u32) -> f32 {
        net.segments[seg as usize].sched_s
    }

    fn name(&self) -> &'static str {
        "timetable"
    }
}

/// Arrival time of every trip at every stop, for one day type.
pub struct Timetable {
    /// Per route: `trips x stops` arrivals, row-major, seconds since midnight.
    /// A zero means "this bus has already passed that stop" (live rows only).
    pub arr: Vec<Vec<u32>>,
    /// Per route and trip: index into `vehicles` when the row is a real bus
    /// being tracked right now. Empty for a purely scheduled timetable.
    pub tracked: Vec<Vec<Option<u32>>>,
    pub vehicles: Vec<String>,
    /// Per vehicle: true when the bus is standing at its first stop and its
    /// row starts at an estimated departure, false when it is under way.
    pub standing: Vec<bool>,
}

/// One scheduled trip's arrivals, each no earlier than the previous row's
/// (buses on one route do not overtake: keeps arrivals at a stop sorted by
/// trip, which the boarding search needs).
fn scheduled_row(net: &Network, times: &dyn SegTimes, day_type: usize, segs: &[u32], start: u32, prev: Option<&[u32]>) -> Vec<u32> {
    let mut row = Vec::with_capacity(segs.len() + 1);
    let mut t = start as f64;
    row.push(start.max(prev.map_or(0, |p| p[0])));
    for (k, &seg) in segs.iter().enumerate() {
        t += times.secs(net, seg, day_type, (t as u32) % 86_400).max(1.0) as f64;
        row.push((t.round() as u32).max(prev.map_or(0, |p| p[k + 1])));
    }
    row
}

impl Timetable {
    /// Every GTFS trip, starting at its timetable time, with later stops
    /// timed by `times`.
    pub fn build(net: &Network, times: &dyn SegTimes, day_type: usize) -> Timetable {
        let arr = net
            .routes
            .par_iter()
            .map(|r| {
                let n = r.stops.len();
                let mut a: Vec<u32> = Vec::with_capacity(n * r.starts.len());
                for &start in &r.starts {
                    let prev = a.len().checked_sub(n).map(|i| a[i..].to_vec());
                    a.extend(scheduled_row(net, times, day_type, &r.segs, start, prev.as_deref()));
                }
                a
            })
            .collect();
        Timetable { arr, tracked: vec![], vehicles: vec![], standing: vec![] }
    }

    /// The timetable as it stands right now. On a route with buses being
    /// tracked, the trips are those real buses (from where each one is,
    /// forward) followed by the scheduled departures still to come; scheduled
    /// trips that "should" already be on the road are dropped, because the
    /// real ones are known. A route with no tracked bus keeps its schedule.
    ///
    /// A bus standing at the route's first stop is the next departure: it
    /// takes the place of the timetabled start nearest to when it is expected
    /// to set off. Only one standing bus per route is used, the one that
    /// arrived most recently; the others are parked for all we can tell.
    pub fn build_live(net: &Network, times: &dyn SegTimes, day_type: usize, now: i64, states: &[VehicleState]) -> Timetable {
        let now_tod = Local::from_epoch(now).tod as f64;
        let mut by_route: HashMap<u32, Vec<u32>> = HashMap::new();
        let mut standing: HashMap<u32, u32> = HashMap::new();
        for (i, s) in states.iter().enumerate() {
            match s.waiting_s {
                None => by_route.entry(s.route).or_default().push(i as u32),
                Some(w) if w <= crate::link::WAITING_MAX_S => {
                    let e = standing.entry(s.route).or_insert(i as u32);
                    if states[*e as usize].waiting_s.is_some_and(|x| w < x) {
                        *e = i as u32;
                    }
                }
                Some(_) => {}
            }
        }
        let built: Vec<(Vec<u32>, Vec<Option<u32>>)> = net
            .routes
            .par_iter()
            .enumerate()
            .map(|(ri, r)| {
                let n = r.stops.len();
                let mut a: Vec<u32> = Vec::new();
                let mut who: Vec<Option<u32>> = Vec::new();
                // Furthest along first: at any stop it is the earliest to arrive.
                let mut vs = by_route.get(&(ri as u32)).cloned().unwrap_or_default();
                vs.sort_by(|x, y| states[*y as usize].along.total_cmp(&states[*x as usize].along));
                for &vi in &vs {
                    let s = &states[vi as usize];
                    // First stop still ahead of the bus.
                    let Some(next) = r.along.iter().position(|&x| x > s.along + 5.0) else { continue };
                    let mut row = vec![0u32; n];
                    let mut t = (now_tod + (s.ts - now) as f64).max(1.0);
                    if next > 0 {
                        // The remainder of the segment the bus is on, pro rata.
                        let span = (r.along[next] - r.along[next - 1]).max(1.0);
                        let frac = ((r.along[next] - s.along) / span).clamp(0.0, 1.0);
                        t += frac * times.secs(net, r.segs[next - 1], day_type, (t as u32) % 86_400) as f64;
                    }
                    row[next] = (t.round() as u32).max(1);
                    for k in next..n - 1 {
                        t += times.secs(net, r.segs[k], day_type, (t as u32) % 86_400).max(1.0) as f64;
                        row[k + 1] = t.round() as u32;
                    }
                    if let Some(i) = a.len().checked_sub(n) {
                        for k in 0..n {
                            if row[k] != 0 && row[k] < a[i + k] {
                                row[k] = a[i + k];
                            }
                        }
                    }
                    a.extend(row);
                    who.push(Some(vi));
                }
                let any_tracked = !who.is_empty();
                let mut starts: Vec<(u32, Option<u32>)> =
                    r.starts.iter().filter(|&&s| !(any_tracked && (s as f64) < now_tod)).map(|&s| (s, None)).collect();
                if let Some(&vi) = standing.get(&(ri as u32)) {
                    let s = &states[vi as usize];
                    let leave = (now_tod + (s.ts - now) as f64 + crate::link::leaves_after(s.waiting_s.unwrap_or(0))).max(1.0) as u32;
                    // The standing bus is one of the timetabled departures: the nearest one.
                    let i = starts.partition_point(|x| x.0 < leave);
                    let gap = |j: usize| starts.get(j).map(|x| x.0.abs_diff(leave));
                    let nearest = match (i.checked_sub(1).and_then(gap), gap(i)) {
                        (Some(b), Some(f)) => Some(if b < f { i - 1 } else { i }),
                        (Some(_), None) => Some(i - 1),
                        (None, Some(_)) => Some(i),
                        (None, None) => None,
                    };
                    match nearest {
                        Some(j) => starts[j] = (leave, Some(vi)),
                        None => starts.push((leave, Some(vi))),
                    }
                    starts.sort_by_key(|x| x.0);
                }
                for (start, bus) in starts {
                    let prev = a.len().checked_sub(n).map(|i| a[i..].to_vec());
                    a.extend(scheduled_row(net, times, day_type, &r.segs, start, prev.as_deref()));
                    who.push(bus);
                }
                (a, who)
            })
            .collect();
        let (arr, tracked) = built.into_iter().unzip();
        Timetable {
            arr,
            tracked,
            vehicles: states.iter().map(|s| s.vehicle.clone()).collect(),
            standing: states.iter().map(|s| s.waiting_s.is_some()).collect(),
        }
    }

    #[inline]
    pub fn at(&self, route: u32, trip: u32, pos: usize, n_stops: usize) -> u32 {
        self.arr[route as usize][trip as usize * n_stops + pos]
    }

    /// The number plate of the bus, when this trip is a real one being tracked.
    pub fn vehicle(&self, route: u32, trip: u32) -> Option<&str> {
        let v = (*self.tracked.get(route as usize)?.get(trip as usize)?)?;
        self.vehicles.get(v as usize).map(String::as_str)
    }

    /// True when this trip is a real bus still standing at its first stop,
    /// so its times start from an estimated departure.
    pub fn is_standing(&self, route: u32, trip: u32) -> bool {
        let v = self.tracked.get(route as usize).and_then(|t| t.get(trip as usize)).copied().flatten();
        v.is_some_and(|v| self.standing.get(v as usize).copied().unwrap_or(false))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Leg {
    /// Ride `route`'s trip number `trip` from stop position `board` to `alight`.
    Ride { route: u32, trip: u32, board: u16, alight: u16, board_time: u32, alight_time: u32 },
    /// Walk between two stops to change buses.
    Transfer { from: u32, to: u32, secs: u32 },
}

#[derive(Clone, Debug)]
pub struct Journey {
    /// First boarding stop and the walk to it, seconds.
    pub access: (u32, u32),
    /// Last alighting stop and the walk from it, seconds.
    pub egress: (u32, u32),
    pub legs: Vec<Leg>,
    /// Arrival at the destination, seconds since midnight.
    pub arrive: u32,
}

impl Journey {
    pub fn rides(&self) -> usize {
        self.legs.iter().filter(|l| matches!(l, Leg::Ride { .. })).count()
    }

    pub fn routes(&self) -> Vec<u32> {
        self.legs.iter().filter_map(|l| if let Leg::Ride { route, .. } = l { Some(*route) } else { None }).collect()
    }
}

const INF: u32 = u32::MAX;
const NONE: u32 = u32::MAX;
/// Minimum time between getting off one bus and boarding the next.
pub const TRANSFER_BUFFER: u32 = 120;

#[derive(Clone, Copy)]
struct RideLabel {
    time: u32,
    route: u32,
    trip: u32,
    board: u16,
    alight: u16,
}

pub struct Search<'a> {
    pub net: &'a Network,
    pub tt: &'a Timetable,
    /// (stop, walk seconds from the origin).
    pub sources: &'a [(u32, u32)],
    /// (stop, walk seconds to the destination).
    pub targets: &'a [(u32, u32)],
    /// Seconds since midnight.
    pub depart: u32,
    /// Maximum number of buses in a journey.
    pub max_rides: usize,
    pub banned: &'a HashSet<u32>,
}

impl Search<'_> {
    /// Earliest-arrival journeys, one per number of rides, keeping only those
    /// where an extra bus actually gets you there sooner.
    pub fn run(&self) -> Vec<Journey> {
        let net = self.net;
        let ns = net.stops.len();
        let rounds = self.max_rides;
        // tau[k][s]: earliest arrival at s using at most k rides.
        let mut tau: Vec<Vec<u32>> = vec![vec![INF; ns]; rounds + 1];
        // Best arrival at s by getting off a bus in exactly round k.
        let mut ride: Vec<Vec<Option<RideLabel>>> = vec![vec![None; ns]; rounds + 1];
        // If the label of s in round k is a walk, the stop walked from.
        let mut walk_from: Vec<Vec<u32>> = vec![vec![NONE; ns]; rounds + 1];
        let mut access = vec![INF; ns];
        let mut marked: Vec<u32> = Vec::new();
        for &(s, w) in self.sources {
            let t = self.depart + w;
            if t < tau[0][s as usize] {
                tau[0][s as usize] = t;
                access[s as usize] = w;
                marked.push(s);
            }
        }
        let mut egress = vec![INF; ns];
        for &(s, w) in self.targets {
            egress[s as usize] = egress[s as usize].min(w);
        }
        let mut best_target = INF;
        let mut is_marked = vec![false; ns];
        let mut first_pos: Vec<u32> = vec![NONE; net.routes.len()];

        for k in 1..=rounds {
            let (prev, cur) = tau.split_at_mut(k);
            let (prev, cur) = (&prev[k - 1], &mut cur[0]);
            cur.copy_from_slice(prev);

            // Routes through a stop improved last round, from the earliest such stop.
            let mut queue: Vec<u32> = Vec::new();
            for &s in &marked {
                for &(r, pos) in &net.stop_routes[s as usize] {
                    if self.banned.contains(&r) {
                        continue;
                    }
                    let fp = &mut first_pos[r as usize];
                    if *fp == NONE {
                        queue.push(r);
                        *fp = pos as u32;
                    } else if (pos as u32) < *fp {
                        *fp = pos as u32;
                    }
                }
            }
            marked.clear();
            is_marked.iter_mut().for_each(|m| *m = false);

            for &r in &queue {
                let route = &net.routes[r as usize];
                let n = route.stops.len();
                let arr = &self.tt.arr[r as usize];
                let n_trips = arr.len() / n;
                let mut cur_trip: Option<usize> = None;
                let mut board_pos = 0usize;
                let start = first_pos[r as usize] as usize;
                first_pos[r as usize] = NONE;
                for pos in start..n {
                    let s = route.stops[pos] as usize;
                    if let Some(t) = cur_trip {
                        let a = arr[t * n + pos];
                        if a < cur[s].min(best_target) && pos > board_pos {
                            cur[s] = a;
                            ride[k][s] = Some(RideLabel { time: a, route: r, trip: t as u32, board: board_pos as u16, alight: pos as u16 });
                            walk_from[k][s] = NONE;
                            if !is_marked[s] {
                                is_marked[s] = true;
                                marked.push(s as u32);
                            }
                            if egress[s] != INF {
                                best_target = best_target.min(a + egress[s]);
                            }
                        }
                    }
                    if prev[s] == INF {
                        continue;
                    }
                    // No buffer for the first bus: the walk to the stop is already counted.
                    let ready = prev[s] + if k > 1 { TRANSFER_BUFFER } else { 0 };
                    if cur_trip.is_none_or(|t| ready <= arr[t * n + pos]) {
                        // Earliest trip reaching this stop at or after `ready`.
                        let (mut lo, mut hi) = (0usize, cur_trip.unwrap_or(n_trips));
                        while lo < hi {
                            let mid = (lo + hi) / 2;
                            if arr[mid * n + pos] < ready { lo = mid + 1 } else { hi = mid }
                        }
                        if lo < n_trips && cur_trip.is_none_or(|t| lo < t) {
                            cur_trip = Some(lo);
                            board_pos = pos;
                        }
                    }
                }
            }

            // Walk from every stop reached by bus this round.
            let by_ride: Vec<u32> = marked.clone();
            for &s in &by_ride {
                let t0 = ride[k][s as usize].map(|l| l.time).unwrap_or(INF);
                if t0 == INF {
                    continue;
                }
                for &(nb, w) in &net.footpaths[s as usize] {
                    let a = t0 + w;
                    let nbu = nb as usize;
                    if a < cur[nbu].min(best_target) {
                        cur[nbu] = a;
                        walk_from[k][nbu] = s;
                        if !is_marked[nbu] {
                            is_marked[nbu] = true;
                            marked.push(nb);
                        }
                        if egress[nbu] != INF {
                            best_target = best_target.min(a + egress[nbu]);
                        }
                    }
                }
            }
            if marked.is_empty() {
                break;
            }
        }

        // One journey per ride count, if it beats every journey with fewer rides.
        let mut out: Vec<Journey> = Vec::new();
        let mut best_so_far = INF;
        for k in 1..=rounds {
            let end = (0..ns)
                .filter(|&s| egress[s] != INF && tau[k][s] != INF)
                .min_by_key(|&s| tau[k][s] + egress[s]);
            let Some(end) = end else { continue };
            let arrive = tau[k][end] + egress[end];
            if arrive >= best_so_far {
                continue;
            }
            if let Some(j) = self.rebuild(k, end, arrive, egress[end], &tau, &ride, &walk_from, &access) {
                best_so_far = arrive;
                out.push(j);
            }
        }
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn rebuild(
        &self,
        k: usize,
        end: usize,
        arrive: u32,
        egress: u32,
        tau: &[Vec<u32>],
        ride: &[Vec<Option<RideLabel>>],
        walk_from: &[Vec<u32>],
        access: &[u32],
    ) -> Option<Journey> {
        let mut legs: Vec<Leg> = Vec::new();
        let (mut k, mut s) = (k, end);
        loop {
            // Drop to the round in which this stop's label was actually set.
            while k > 0 && tau[k][s] == tau[k - 1][s] {
                k -= 1;
            }
            if k == 0 {
                break;
            }
            if walk_from[k][s] != NONE {
                let from = walk_from[k][s] as usize;
                let l = ride[k][from]?;
                legs.push(Leg::Transfer { from: from as u32, to: s as u32, secs: tau[k][s] - l.time });
                s = from;
            }
            let l = ride[k][s]?;
            let route = &self.net.routes[l.route as usize];
            let n = route.stops.len();
            legs.push(Leg::Ride {
                route: l.route,
                trip: l.trip,
                board: l.board,
                alight: l.alight,
                board_time: self.tt.at(l.route, l.trip, l.board as usize, n),
                alight_time: l.time,
            });
            s = route.stops[l.board as usize] as usize;
            k -= 1;
        }
        if legs.is_empty() || access[s] == INF {
            return None;
        }
        legs.reverse();
        Some(Journey { access: (s as u32, access[s]), egress: (end as u32, egress), legs, arrive })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::Polyline;
    use crate::gtfs::{Route, Segment, Stop};

    /// Two routes of 500 m hops at 100 s each. Route 0 runs stops 0-1-2-3,
    /// route 1 runs stops 4-5 where stop 4 is a 60 s walk from stop 2.
    fn toy() -> Network {
        let stops: Vec<Stop> =
            (0..6).map(|i| Stop { id: format!("s{i}"), name: format!("S{i}"), lat: 0.0, lon: 0.0, xy: (i as f64 * 500.0, 0.0) }).collect();
        let route = |id: &str, st: Vec<u32>, segs: Vec<u32>, starts: Vec<u32>| Route {
            id: id.into(),
            name: format!("{id}UP"),
            desc: String::new(),
            agency: String::new(),
            sched: (0..st.len() as u32).map(|i| i * 100).collect(),
            shape: Polyline::new(vec![(0.0, 0.0), (500.0 * (st.len() - 1) as f64, 0.0)]),
            along: (0..st.len()).map(|i| i as f64 * 500.0).collect(),
            stops: st,
            starts,
            segs,
        };
        let routes = vec![route("a", vec![0, 1, 2, 3], vec![0, 1, 2], vec![1000, 1600, 2200]), route("b", vec![4, 5], vec![3], vec![1300, 1900])];
        let segments: Vec<Segment> = [(0, 1), (1, 2), (2, 3), (4, 5)].iter().map(|&(a, b)| Segment { a, b, len_m: 500.0, sched_s: 100.0, n_routes: 1 }).collect();
        let mut stop_routes = vec![vec![]; 6];
        for (ri, r) in routes.iter().enumerate() {
            for (pos, &s) in r.stops.iter().enumerate() {
                stop_routes[s as usize].push((ri as u32, pos as u16));
            }
        }
        let mut footpaths = vec![vec![]; 6];
        footpaths[2].push((4, 60));
        footpaths[4].push((2, 60));
        Network {
            stops,
            routes,
            segments,
            stop_index: Default::default(),
            route_index: Default::default(),
            seg_index: Default::default(),
            stop_routes,
            footpaths,
            seg_neighbours: vec![vec![]; 4],
            siblings: vec![None, None],
        }
    }

    fn search(net: &Network, tt: &Timetable, from: u32, to: u32, depart: u32) -> Vec<Journey> {
        Search { net, tt, sources: &[(from, 0)], targets: &[(to, 0)], depart, max_rides: 3, banned: &HashSet::new() }.run()
    }

    #[test]
    fn direct_ride_takes_the_first_bus_that_can_be_caught() {
        let net = toy();
        let tt = Timetable::build(&net, &Scheduled, 0);
        // At stop 1 the buses pass at 1100, 1700, 2300. Arriving there at 1150 means the 1700 one.
        let j = &search(&net, &tt, 1, 3, 1150)[0];
        assert_eq!(j.arrive, 1900);
        assert_eq!(j.legs, vec![Leg::Ride { route: 0, trip: 1, board: 1, alight: 3, board_time: 1700, alight_time: 1900 }]);
    }

    #[test]
    fn transfer_walks_between_stops_and_respects_the_buffer() {
        let net = toy();
        let tt = Timetable::build(&net, &Scheduled, 0);
        // Ride a to stop 2 (arrive 1200), walk 60 s to stop 4 (1260). Route b leaves at 1300,
        // but 1260 + the 120 s buffer = 1380, so it has to be the 1900 bus.
        let j = &search(&net, &tt, 0, 5, 900)[0];
        assert_eq!(j.rides(), 2);
        assert_eq!(j.arrive, 2000);
        assert!(matches!(j.legs[1], Leg::Transfer { from: 2, to: 4, secs: 60 }));
    }

    #[test]
    fn live_timetable_boards_real_buses_and_never_at_a_stop_they_have_passed() {
        let net = toy();
        let now = Local::parse("2026-11-17T00:20").unwrap().epoch(); // tod = 1200
        // One real bus on route a, 750 m along (half way between stops 1 and 2), seen 10 s ago.
        let states = vec![VehicleState { vehicle: "DL1".into(), route: 0, along: 750.0, ts: now - 10, waiting_s: None }];
        let tt = Timetable::build_live(&net, &Scheduled, 0, now, &states);
        // Row 0 is the real bus: passed stops 0 and 1, reaches stop 2 in half a segment (50 s) from 1190.
        assert_eq!(&tt.arr[0][0..4], &[0, 0, 1240, 1340]);
        assert_eq!(tt.vehicle(0, 0), Some("DL1"));
        // Scheduled trips that should already be out (1000) are dropped; later ones stay.
        assert_eq!(tt.arr[0].len() / 4, 3);
        assert_eq!(tt.vehicle(0, 1), None);
        // Waiting at stop 2 at 1200: the real bus comes at 1240.
        let j = &search(&net, &tt, 2, 3, 1200)[0];
        assert_eq!((j.arrive, j.legs.len()), (1340, 1));
        // Waiting at stop 1: the real bus has gone; next is the 1600 departure, at stop 1 by 1700.
        let j = &search(&net, &tt, 1, 3, 1200)[0];
        assert_eq!(j.arrive, 1900);
        // Route b has no tracked bus, so it keeps its whole schedule.
        assert_eq!(tt.arr[1].len() / 2, 2);
    }

    #[test]
    fn a_bus_standing_at_its_first_stop_replaces_the_nearest_timetabled_start() {
        let net = toy();
        let now = Local::parse("2026-11-17T00:20").unwrap().epoch(); // tod = 1200
        let bus = |name: &str, waited: i64| VehicleState { vehicle: name.into(), route: 1, along: 0.0, ts: now - 10, waiting_s: Some(waited) };
        // Route b starts at 1300 and 1900. A bus has stood at its first stop for 60 s:
        // expected off 120 + 0.9 x 60 = 174 s after it was last seen, at 1364.
        let tt = Timetable::build_live(&net, &Scheduled, 0, now, &[bus("DL2", 60)]);
        assert_eq!(tt.arr[1], vec![1364, 1464, 1900, 2000]);
        assert_eq!(tt.vehicle(1, 0), Some("DL2"));
        assert!(tt.is_standing(1, 0) && !tt.is_standing(1, 1));
        // Of two standing buses only the more recent arrival is used; one parked for an hour is not.
        let tt = Timetable::build_live(&net, &Scheduled, 0, now, &[bus("OLD", 900), bus("NEW", 0), bus("PARKED", 3600)]);
        assert_eq!(tt.arr[1], vec![1310, 1410, 1900, 2000]);
        assert_eq!(tt.vehicle(1, 0), Some("NEW"));
        let tt = Timetable::build_live(&net, &Scheduled, 0, now, &[bus("PARKED", 3600)]);
        assert_eq!((tt.arr[1].clone(), tt.vehicle(1, 0)), (vec![1300, 1400, 1900, 2000], None));
    }
}
