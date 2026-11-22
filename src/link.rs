//! GPS points -> distance along the route -> stop crossing times -> segment
//! traversals. This is the "how GPS is linked to routes" step of the
//! technical note.

use crate::clock::Local;
use crate::geom::{Cand, P, to_xy};
use crate::gtfs::{Network, Route};
use crate::rt;
use anyhow::{Context, Result};
use rayon::prelude::*;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// A GPS point further than this from its route's shape is off-route.
pub const MAX_OFFSET_M: f64 = 60.0;
/// A silence longer than this ends a run.
pub const RUN_GAP_S: i64 = 600;
/// A stop crossing interpolated across a longer gap than this is not trusted.
pub const MAX_BRACKET_S: f64 = 120.0;
/// Faster than this between two fixes is a GPS jump, not a bus.
pub const MAX_SPEED: f64 = 30.0;
/// Plausible average speed over a segment, m/s (1.4 to 90 km/h).
pub const SEG_SPEED: (f64, f64) = (0.4, 25.0);
/// A terminal is "left" once the bus is this far past the first stop.
const DEPART_M: f64 = 25.0;
/// A stop is "reached" this far before its position, so dwell at a stop
/// belongs to the segment that starts there.
const ARRIVE_M: f64 = 15.0;

#[derive(Clone, Copy)]
struct Fix {
    ts: i64,
    xy: P,
    route: u32,
    trip: u32,
}

pub const NO_ROUTE: u32 = u32::MAX;

#[derive(Default, Clone, Debug)]
pub struct Stats {
    pub snapshots: usize,
    pub vehicles: usize,
    pub fixes: usize,
    pub fixes_unknown_route: usize,
    pub runs: usize,
    pub fixes_in_runs: usize,
    pub fixes_on_route: usize,
    pub crossings: usize,
    pub traversals: usize,
    pub rejected_gap: usize,
    pub rejected_speed: usize,
    /// Runs that only made sense on the opposite-direction route and were re-tagged.
    pub relabelled: usize,
}

impl Stats {
    fn add(&mut self, o: &Stats) {
        self.runs += o.runs;
        self.fixes_in_runs += o.fixes_in_runs;
        self.fixes_on_route += o.fixes_on_route;
        self.crossings += o.crossings;
        self.traversals += o.traversals;
        self.rejected_gap += o.rejected_gap;
        self.rejected_speed += o.rejected_speed;
        self.relabelled += o.relabelled;
    }
}

/// One bus covering one stop-to-stop segment.
#[derive(Clone, Debug)]
pub struct Traversal {
    pub seg: u32,
    pub route: u32,
    /// Position of the segment's first stop in the route.
    pub k: u16,
    /// Epoch seconds at which the bus reached the first stop.
    pub t_enter: f64,
    pub dur: f32,
    /// Longest GPS gap either crossing was interpolated over.
    pub gap: f32,
}

/// Choose one position per fix so the bus moves forward at plausible speeds.
/// With a single candidate per fix (the usual case) this is a no-op; it
/// matters on loops and out-and-back roads where a point is near two passes.
fn viterbi(cands: &[Vec<Cand>], ts: &[i64]) -> Vec<f64> {
    let n = cands.len();
    let mut cost: Vec<Vec<f64>> = vec![cands[0].iter().map(|c| c.d).collect()];
    let mut back: Vec<Vec<usize>> = vec![vec![0; cands[0].len()]];
    for i in 1..n {
        let dt = (ts[i] - ts[i - 1]).max(1) as f64;
        let (mut ci, mut bi) = (Vec::with_capacity(cands[i].len()), Vec::with_capacity(cands[i].len()));
        for c in &cands[i] {
            let (mut best, mut arg) = (f64::INFINITY, 0);
            for (j, p) in cands[i - 1].iter().enumerate() {
                let step = c.along - p.along;
                let pen = if step < -30.0 {
                    300.0 - step
                } else if step / dt > MAX_SPEED {
                    300.0 + step
                } else {
                    0.0
                };
                let v = cost[i - 1][j] + pen;
                if v < best {
                    best = v;
                    arg = j;
                }
            }
            ci.push(best + c.d);
            bi.push(arg);
        }
        cost.push(ci);
        back.push(bi);
    }
    let mut j = (0..cands[n - 1].len()).min_by(|&a, &b| cost[n - 1][a].total_cmp(&cost[n - 1][b])).unwrap();
    let mut out = vec![0.0; n];
    for i in (0..n).rev() {
        out[i] = cands[i][j].along;
        j = back[i][j];
    }
    out
}

/// Fixes of a run that lie on the route, as (timestamps, metres along the route).
fn on_route(route: &Route, fixes: &[Fix]) -> (Vec<i64>, Vec<f64>) {
    let mut ts: Vec<i64> = Vec::new();
    let mut cands: Vec<Vec<Cand>> = Vec::new();
    for f in fixes {
        let c = route.shape.candidates(f.xy, MAX_OFFSET_M);
        if !c.is_empty() {
            ts.push(f.ts);
            cands.push(c);
        }
    }
    if ts.is_empty() {
        return (ts, vec![]);
    }
    let along = viterbi(&cands, &ts);
    (ts, along)
}

/// Turn one run (one vehicle, one dispatch, one route) into traversals.
fn link_run(net: &Network, route_idx: u32, fixes: &[Fix], st: &mut Stats, out: &mut Vec<Traversal>) {
    let route: &Route = &net.routes[route_idx as usize];
    st.runs += 1;
    st.fixes_in_runs += fixes.len();
    let (ts, along) = on_route(route, fixes);
    st.fixes_on_route += ts.len();
    if ts.len() < 2 {
        return;
    }

    // Stop thresholds along the route.
    let n = route.stops.len();
    let mut thr: Vec<f64> = (0..n).map(|k| if k == 0 { route.along[0] + DEPART_M } else { route.along[k] - ARRIVE_M }).collect();
    for k in 1..n {
        if thr[k] < thr[k - 1] {
            thr[k] = thr[k - 1];
        }
    }

    // First time the (monotone) position passes each threshold.
    let mut cross: Vec<Option<(f64, f64)>> = vec![None; n]; // (time, bracketing gap)
    let mut k = 0;
    let mut a0 = along[0];
    while k < n && thr[k] <= a0 {
        k += 1; // already past these stops when first seen
    }
    for i in 1..along.len() {
        let a1 = along[i].max(a0);
        let dt = (ts[i] - ts[i - 1]) as f64;
        if a1 > a0 {
            let jump = dt <= 0.0 || (a1 - a0) / dt > MAX_SPEED;
            while k < n && thr[k] <= a1 {
                if !jump {
                    let t = ts[i - 1] as f64 + (thr[k] - a0) / (a1 - a0) * dt;
                    cross[k] = Some((t, dt));
                    st.crossings += 1;
                }
                k += 1;
            }
        }
        a0 = a1;
    }

    for k in 0..n - 1 {
        let (Some((t0, g0)), Some((t1, g1))) = (cross[k], cross[k + 1]) else { continue };
        let gap = g0.max(g1);
        if gap > MAX_BRACKET_S {
            st.rejected_gap += 1;
            continue;
        }
        let dur = t1 - t0;
        let len = net.segments[route.segs[k] as usize].len_m as f64;
        if dur < 0.0 || (len >= 30.0 && (dur <= 0.0 || len / dur < SEG_SPEED.0 || len / dur > SEG_SPEED.1)) {
            st.rejected_speed += 1;
            continue;
        }
        st.traversals += 1;
        out.push(Traversal { seg: route.segs[k], route: route_idx, k: k as u16, t_enter: t0, dur: dur as f32, gap: gap as f32 });
    }
}

/// Per-vehicle tracks from a set of snapshots, de-duplicated on timestamp.
struct Tracks {
    vehicles: Vec<String>,
    trips: Vec<String>,
    fixes: Vec<Vec<Fix>>,
    stats: Stats,
}

/// Collects GPS fixes into per-vehicle tracks, whatever the source.
struct TrackBuilder {
    vidx: HashMap<String, u32>,
    tidx: HashMap<String, u32>,
    t: Tracks,
}

impl TrackBuilder {
    fn new() -> Self {
        TrackBuilder { vidx: HashMap::new(), tidx: HashMap::new(), t: Tracks { vehicles: vec![], trips: vec![], fixes: vec![], stats: Stats::default() } }
    }

    /// `route` is an index into the network's routes, or `NO_ROUTE`.
    fn push(&mut self, vehicle: &str, route: u32, trip: &str, lat: f64, lon: f64, ts: i64) {
        let vi = match self.vidx.get(vehicle) {
            Some(&i) => i,
            None => {
                let i = self.t.vehicles.len() as u32;
                self.vidx.insert(vehicle.to_string(), i);
                self.t.vehicles.push(vehicle.to_string());
                self.t.fixes.push(Vec::new());
                i
            }
        } as usize;
        // Feeds repeat a bus's last position until it reports again.
        if self.t.fixes[vi].last().is_some_and(|f| f.ts == ts) {
            return;
        }
        let ti = match self.tidx.get(trip) {
            Some(&i) => i,
            None => {
                let i = self.t.trips.len() as u32;
                self.tidx.insert(trip.to_string(), i);
                self.t.trips.push(trip.to_string());
                i
            }
        };
        self.t.fixes[vi].push(Fix { ts, xy: to_xy(lat, lon), route, trip: ti });
    }

    fn finish(mut self) -> Tracks {
        let t = &mut self.t;
        for f in t.fixes.iter_mut() {
            f.sort_by_key(|x| x.ts);
            f.dedup_by_key(|x| x.ts);
        }
        t.stats.vehicles = t.vehicles.len();
        t.stats.fixes = t.fixes.iter().map(|f| f.len()).sum();
        t.stats.fixes_unknown_route = t.fixes.iter().flatten().filter(|f| f.route == NO_ROUTE).count();
        self.t
    }
}

fn load_tracks(net: &Network, files: &[(i64, PathBuf)]) -> Result<Tracks> {
    let decoded: Vec<Vec<rt::Veh>> = files
        .par_iter()
        .map(|(_, p)| rt::read_snapshot(p))
        .collect::<Result<Vec<_>>>()?;
    let mut b = TrackBuilder::new();
    b.t.stats.snapshots = files.len();
    for snap in decoded {
        for v in snap {
            let route = net.route_index.get(&v.route).copied().unwrap_or(NO_ROUTE);
            b.push(&v.id, route, &v.trip, v.lat as f64, v.lon as f64, v.ts);
        }
    }
    Ok(b.finish())
}

/// Per-vehicle tracks from one day of the organisers' historical archive: a
/// SQLite database with one row per bus per poll. Same fields as the live feed, so everything downstream is shared.
fn load_tracks_db(net: &Network, db: &Path, thin: bool) -> Result<Tracks> {
    use rusqlite::types::ValueRef;
    use rusqlite::{Connection, OpenFlags};
    // The files are in WAL mode without their -wal file; `immutable` lets
    // SQLite read them without trying to create one.
    let enc: String = db.to_string_lossy().chars().map(|c| if matches!(c, '%' | '?' | '#') { format!("%{:02X}", c as u32) } else { c.to_string() }).collect();
    let conn = Connection::open_with_flags(format!("file:{enc}?immutable=1"), OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI)
        .with_context(|| format!("opening {}", db.display()))?;
    let mut stmt = conn.prepare("SELECT vehicle_id, lat, lng, route_id, trip_id, timestamp, time FROM vehicle_feed")?;
    let mut rows = stmt.query([])?;
    // Columns are loosely typed (coordinates and timestamps are text, the route id is a real).
    fn num(v: ValueRef) -> Option<f64> {
        match v {
            ValueRef::Real(x) => Some(x),
            ValueRef::Integer(x) => Some(x as f64),
            ValueRef::Text(t) => std::str::from_utf8(t).ok()?.trim().parse().ok(),
            _ => None,
        }
    }
    fn text(v: ValueRef<'_>) -> &str {
        match v {
            ValueRef::Text(t) => std::str::from_utf8(t).unwrap_or(""),
            _ => "",
        }
    }
    let mut b = TrackBuilder::new();
    let mut route_of: HashMap<i64, u32> = HashMap::new();
    let mut polls: std::collections::HashSet<[u8; 8]> = std::collections::HashSet::new();
    while let Some(r) = rows.next()? {
        let (Some(lat), Some(lon), Some(ts)) = (num(r.get_ref(1)?), num(r.get_ref(2)?), num(r.get_ref(5)?)) else { continue };
        let route = match num(r.get_ref(3)?) {
            Some(id) => *route_of.entry(id as i64).or_insert_with(|| net.route_index.get(&(id as i64).to_string()).copied().unwrap_or(NO_ROUTE)),
            None => NO_ROUTE,
        };
        if let Ok(t) = <[u8; 8]>::try_from(text(r.get_ref(6)?).as_bytes()) {
            // "HH:MM:SS". Thinning keeps polls 20 and 30 s apart by turns,
            // which is what our own collector sees of a feed that refreshes
            // every 24 s or so.
            let ten_s = ((t[3] - b'0') as u32 * 10 + (t[4] - b'0') as u32) * 6 + (t[6] - b'0') as u32;
            if thin && !matches!(ten_s % 5, 0 | 2) {
                continue;
            }
            polls.insert(t);
        }
        b.push(text(r.get_ref(0)?), route, text(r.get_ref(4)?), lat, lon, ts as i64);
    }
    b.t.stats.snapshots = polls.len();
    Ok(b.finish())
}

/// Split one vehicle's fixes into runs and link each; calls `emit` with every
/// traversal and the index of the run's dispatch id.
fn vehicle_traversals(net: &Network, fixes: &[Fix], st: &mut Stats, mut emit: impl FnMut(&Traversal, u32)) {
    let mut trav: Vec<Traversal> = Vec::new();
    let mut start = 0;
    for i in 1..=fixes.len() {
        let boundary = i == fixes.len()
            || fixes[i].route != fixes[start].route
            || fixes[i].trip != fixes[start].trip
            || fixes[i].ts - fixes[i - 1].ts > RUN_GAP_S;
        if !boundary {
            continue;
        }
        let run = &fixes[start..i];
        if run[0].route != NO_ROUTE && run.len() >= 3 {
            trav.clear();
            link_run(net, run[0].route, run, st, &mut trav);
            // A bus tagged UP but driving the DOWN route yields nothing on its
            // own route and a clean run on the sibling: re-tag it.
            if trav.len() < 2 {
                if let Some(sib) = net.sibling(run[0].route) {
                    let (mut alt, mut st2) = (Vec::new(), Stats::default());
                    link_run(net, sib, run, &mut st2, &mut alt);
                    if alt.len() >= 4 {
                        st.traversals = st.traversals - trav.len() + alt.len();
                        st.relabelled += 1;
                        trav = alt;
                    }
                }
            }
            for t in &trav {
                emit(t, run[0].trip);
            }
        }
        start = i;
    }
}

/// Extract every traversal from a set of snapshots. Returns CSV text (with
/// header) and statistics.
pub fn extract(net: &Network, files: &[(i64, PathBuf)]) -> Result<(String, Stats)> {
    Ok(traversals_csv(net, &load_tracks(net, files)?))
}

fn traversals_csv(net: &Network, tracks: &Tracks) -> (String, Stats) {
    let per_vehicle: Vec<(String, Stats)> = tracks
        .fixes
        .par_iter()
        .enumerate()
        .map(|(vi, fixes)| {
            let mut st = Stats::default();
            let mut csv = String::new();
            vehicle_traversals(net, fixes, &mut st, |t, trip| {
                let s = &net.segments[t.seg as usize];
                let _ = writeln!(
                    csv,
                    "{:.1},{:.1},{},{},{},{},{:.0},{:.0},{},{}",
                    t.t_enter,
                    t.dur,
                    net.stops[s.a as usize].id,
                    net.stops[s.b as usize].id,
                    net.routes[t.route as usize].id,
                    t.k,
                    s.len_m,
                    t.gap,
                    tracks.vehicles[vi],
                    tracks.trips[trip as usize]
                );
            });
            (csv, st)
        })
        .collect();
    let mut stats = tracks.stats.clone();
    let mut csv = String::from("t_enter,dur_s,stop_a,stop_b,route_id,k,len_m,gap_s,vehicle,trip_id\n");
    for (c, s) in &per_vehicle {
        csv.push_str(c);
        stats.add(s);
    }
    (csv, stats)
}

/// A bus that is on a run right now: where it was last seen along its route.
#[derive(Clone, Debug)]
pub struct VehicleState {
    pub vehicle: String,
    pub route: u32,
    /// Metres along the route at `ts`.
    pub along: f64,
    /// Device time of that position, epoch seconds.
    pub ts: i64,
    /// `None` for a bus on its way along the route. `Some(seconds)` for a bus
    /// standing at the route's first stop that has not set off: how long it
    /// has been there.
    pub waiting_s: Option<i64>,
}

/// A position older than this is not "now".
pub const LIVE_FIX_MAX_AGE_S: i64 = 300;
/// A bus this close to its route's first stop, and not yet under way, is
/// standing at it (terminals are often off the route's line, so this is a
/// straight-line distance, not a position along the route).
pub const FIRST_STOP_M: f64 = 200.0;
/// A standing bus must have reported this recently to count as still there.
const WAITING_FIX_MAX_AGE_S: i64 = 120;
/// A bus that has stood at its first stop longer than this is parked, not
/// about to leave.
pub const WAITING_MAX_S: i64 = 30 * 60;

/// How long after its last position a bus standing at its first stop is
/// expected to set off, given how long it has stood there. Fitted on one day
/// of the feed: the median remaining wait is about 2 minutes for a bus that
/// has just arrived and about 10 for one that has stood 10 minutes or more.
pub fn leaves_after(waiting_s: i64) -> f64 {
    (120.0 + 0.9 * waiting_s as f64).clamp(120.0, 600.0)
}

/// Where a vehicle is on its current run, if it is clearly in service: seen
/// recently, on its route, past its first stop, moving, and not yet at the end.
fn current_state(net: &Network, fixes: &[Fix], name: &str, now: i64) -> Option<VehicleState> {
    let last = fixes.last()?;
    if last.route == NO_ROUTE || now - last.ts > LIVE_FIX_MAX_AGE_S {
        return None;
    }
    // The current run: walk back while the dispatch is the same and there is no long silence.
    let mut start = fixes.len() - 1;
    while start > 0
        && fixes[start - 1].route == last.route
        && fixes[start - 1].trip == last.trip
        && fixes[start].ts - fixes[start - 1].ts <= RUN_GAP_S
    {
        start -= 1;
    }
    let route = &net.routes[last.route as usize];
    let run = &fixes[start..];
    let (ts, along) = on_route(route, run);
    let end = *route.along.last()?;
    let mut furthest = f64::MIN;
    if let (Some(&t_last), Some(&a_raw)) = (ts.last(), along.last()) {
        if ts.len() >= 3 && last.ts - t_last <= 90 {
            let a_last = along.iter().copied().fold(f64::MIN, f64::max).min(a_raw.max(along[along.len() - 2]));
            let a_min = along.iter().copied().fold(f64::MAX, f64::min);
            if a_last - a_min >= 150.0 && a_last > route.along[0] + DEPART_M && a_last < end - 50.0 {
                return Some(VehicleState { vehicle: name.to_string(), route: last.route, along: a_last, ts: t_last, waiting_s: None });
            }
            furthest = a_last;
        }
    }
    // Not under way. Is it standing at the first stop, about to start?
    let first = net.stops[*route.stops.first()? as usize].xy;
    let here = |f: &Fix| crate::geom::dist(f.xy, first) <= FIRST_STOP_M;
    if now - last.ts > WAITING_FIX_MAX_AGE_S || !here(last) || furthest >= end - 300.0 {
        return None; // elsewhere, silent, or at the end of a run that loops back to its start
    }
    let mut i = run.len() - 1;
    while i > 0 && here(&run[i - 1]) && run[i].ts - run[i - 1].ts <= LIVE_FIX_MAX_AGE_S {
        i -= 1;
    }
    Some(VehicleState { vehicle: name.to_string(), route: last.route, along: route.along[0], ts: last.ts, waiting_s: Some(last.ts - run[i].ts) })
}

/// One pass over a set of snapshots: every traversal in them, and where each
/// bus is now if it is under way or standing at its first stop. This is what
/// the live view is built from.
pub fn scan(net: &Network, files: &[(i64, PathBuf)], now: i64) -> Result<(Vec<Traversal>, Vec<VehicleState>)> {
    let tracks = load_tracks(net, files)?;
    let trav = tracks
        .fixes
        .par_iter()
        .flat_map_iter(|fixes| {
            let mut out = Vec::new();
            vehicle_traversals(net, fixes, &mut Stats::default(), |t, _| out.push(t.clone()));
            out
        })
        .collect();
    let states = tracks
        .fixes
        .par_iter()
        .enumerate()
        .filter_map(|(vi, fixes)| current_state(net, fixes, &tracks.vehicles[vi], now))
        .collect();
    Ok((trav, states))
}

/// Extract one archived IST date into `out_dir/<date>.csv`.
pub fn extract_date(net: &Network, raw_dir: &Path, out_dir: &Path, date: &str) -> Result<Stats> {
    let files = rt::list_day(raw_dir, date)?;
    let (csv, stats) = extract(net, &files)?;
    write_day(out_dir, date, &csv)?;
    Ok(stats)
}

/// Extract one day of the historical archive into `out_dir/<date>.csv`.
/// `thin` drops polls to mimic our own collector's coarser sampling; it is
/// for checking that both sources measure the same thing.
pub fn extract_db(net: &Network, db: &Path, out_dir: &Path, date: &str, thin: bool) -> Result<Stats> {
    let (csv, stats) = traversals_csv(net, &load_tracks_db(net, db, thin)?);
    write_day(out_dir, date, &csv)?;
    Ok(stats)
}

fn write_day(out_dir: &Path, date: &str, csv: &str) -> Result<()> {
    std::fs::create_dir_all(out_dir)?;
    let tmp = out_dir.join(format!(".{date}.tmp"));
    std::fs::write(&tmp, csv)?;
    std::fs::rename(&tmp, out_dir.join(format!("{date}.csv")))?;
    Ok(())
}

/// Today's date in IST, for deciding which day is still being collected.
pub fn today() -> String {
    Local::from_epoch(crate::clock::now_epoch()).date_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::Polyline;
    use crate::gtfs::{Segment, Stop};

    /// A straight 2 km route with stops every 500 m.
    fn toy() -> Network {
        let stops: Vec<Stop> = (0..5)
            .map(|i| {
                let xy = (i as f64 * 500.0, 0.0);
                Stop { id: format!("s{i}"), name: format!("S{i}"), lat: 0.0, lon: 0.0, xy }
            })
            .collect();
        let segments: Vec<Segment> =
            (0..4).map(|i| Segment { a: i, b: i + 1, len_m: 500.0, sched_s: 100.0, n_routes: 1 }).collect();
        let route = Route {
            id: "r".into(),
            name: "1UP".into(),
            desc: String::new(),
            agency: String::new(),
            stops: (0..5).collect(),
            sched: (0..5).map(|i| i * 100).collect(),
            starts: vec![0],
            main: 0,
            shape: Polyline::new(vec![(0.0, 0.0), (2000.0, 0.0)]),
            along: (0..5).map(|i| i as f64 * 500.0).collect(),
            segs: (0..4).collect(),
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
            seg_neighbours: vec![],
            siblings: vec![],
            variants: vec![],
        }
    }

    #[test]
    fn constant_speed_bus_gives_exact_segment_times() {
        let net = toy();
        // 10 m/s, a fix every 20 s, starting parked at the terminal.
        let mut fixes = vec![Fix { ts: 0, xy: (0.0, 3.0), route: 0, trip: 0 }];
        for i in 0..=11 {
            fixes.push(Fix { ts: 1000 + i * 20, xy: (i as f64 * 200.0, 3.0), route: 0, trip: 0 });
        }
        let (mut st, mut out) = (Stats::default(), Vec::new());
        link_run(&net, 0, &fixes, &mut st, &mut out);
        assert_eq!(out.len(), 4, "{out:?}");
        // Segments 1..3 run arrival-to-arrival: exactly 500 m at 10 m/s.
        for t in &out[1..] {
            assert!((t.dur - 50.0).abs() < 0.01, "{t:?}");
        }
        // The first runs from leaving the terminal (25 m past) to 15 m before stop 1.
        assert!((out[0].dur - 46.0).abs() < 0.01, "{:?}", out[0]);
        // Telescoping: the sum is the end-to-end time between the outer thresholds.
        let total: f32 = out.iter().map(|t| t.dur).sum();
        assert!((total - 196.0).abs() < 0.05);
    }

    #[test]
    fn dwell_is_charged_to_the_segment_starting_at_that_stop() {
        let net = toy();
        let mut fixes = vec![];
        let mut t = 0;
        // Drive to stop 1 (x=500), sit there 60 s, drive on.
        for x in [100.0, 300.0, 500.0, 500.0, 500.0, 500.0, 700.0, 900.0, 1100.0] {
            fixes.push(Fix { ts: t, xy: (x, 0.0), route: 0, trip: 0 });
            t += 20;
        }
        let (mut st, mut out) = (Stats::default(), Vec::new());
        link_run(&net, 0, &fixes, &mut st, &mut out);
        let s1 = out.iter().find(|t| t.k == 1).unwrap();
        // 500 m at 10 m/s = 50 s, plus the 60 s dwell.
        assert!((s1.dur - 110.0).abs() < 0.01, "{s1:?}");
    }

    #[test]
    fn current_state_reports_buses_under_way_and_buses_standing_at_the_first_stop() {
        let net = toy();
        let fx = |ts: i64, x: f64| Fix { ts, xy: (x, 2.0), route: 0, trip: 0 };
        // Moving along the route, last seen 30 s ago at 900 m.
        let moving: Vec<Fix> = (0..=9).map(|i| fx(1000 + i * 20, i as f64 * 100.0)).collect();
        let s = current_state(&net, &moving, "bus", 1210).unwrap();
        assert!((s.along - 900.0).abs() < 1.0 && s.ts == 1180 && s.route == 0);
        // Same bus, but the data is ten minutes old.
        assert!(current_state(&net, &moving, "bus", 1180 + 601).is_none());
        // Standing at the first stop, never left: reported as waiting there, with how long.
        let parked: Vec<Fix> = (0..=9).map(|i| fx(1000 + i * 20, 3.0)).collect();
        let s = current_state(&net, &parked, "bus", 1200).unwrap();
        assert_eq!((s.waiting_s, s.ts), (Some(180), 1180));
        // The same bus, silent for three minutes: no longer known to be there.
        assert!(current_state(&net, &parked, "bus", 1180 + 181).is_none());
        // Arrived at the first stop from elsewhere a minute ago.
        let mut arriving: Vec<Fix> = (0..=5).map(|i| Fix { ts: 1000 + i * 20, xy: (-900.0 + i as f64 * 150.0, 400.0), route: 0, trip: 0 }).collect();
        arriving.extend((6..=9).map(|i| fx(1000 + i * 20, 3.0)));
        assert_eq!(current_state(&net, &arriving, "bus", 1200).unwrap().waiting_s, Some(60));
        // Reached the last stop: nothing left to ride.
        let done: Vec<Fix> = (0..=10).map(|i| fx(1000 + i * 20, i as f64 * 200.0)).collect();
        assert!(current_state(&net, &done, "bus", 1210).is_none());
    }

    #[test]
    fn gps_jump_and_long_gap_do_not_produce_traversals() {
        let net = toy();
        let fixes = vec![
            Fix { ts: 0, xy: (100.0, 0.0), route: 0, trip: 0 },
            Fix { ts: 20, xy: (300.0, 0.0), route: 0, trip: 0 },
            Fix { ts: 22, xy: (1900.0, 0.0), route: 0, trip: 0 }, // 800 m/s
        ];
        let (mut st, mut out) = (Stats::default(), Vec::new());
        link_run(&net, 0, &fixes, &mut st, &mut out);
        assert!(out.is_empty());
        let fixes = vec![
            Fix { ts: 0, xy: (100.0, 0.0), route: 0, trip: 0 },
            Fix { ts: 400, xy: (700.0, 0.0), route: 0, trip: 0 }, // 400 s silence
            Fix { ts: 420, xy: (1200.0, 0.0), route: 0, trip: 0 },
        ];
        let (mut st, mut out) = (Stats::default(), Vec::new());
        link_run(&net, 0, &fixes, &mut st, &mut out);
        assert!(out.is_empty() && st.rejected_gap == 1, "{out:?} {st:?}");
    }
}
