//! The validation comparison: for each origin-destination pair and departure
//! time, our estimate, Google's, and what real buses did. Also a check on whether buses leave terminals when the
//! timetable says they do.
//!
//! To keep the comparison honest the model used for a slot is fitted only on
//! traversals that finished before that slot: it never sees the buses it is
//! being compared with.

use crate::clock::Local;
use crate::eval::Row;
use crate::google::Pair;
use crate::link::Traversal;
use crate::live::{Blended, LiveRatios, WINDOW_S};
use crate::gtfs::Network;
use crate::model::Model;
use crate::planner::{Engine, LegOut, plan};
use crate::raptor::{Scheduled, SegTimes};
use anyhow::Result;
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;

/// Real buses count as "the same trip" if they boarded within this long after
/// the slot's departure time. Only buses boarding *after* the slot are used:
/// earlier ones are in the model's training data.
const OBSERVED_WINDOW_S: f64 = 5400.0;
/// A slot is compared once this much time has passed since it, so that the
/// buses in its window have had time to finish their rides.
const SETTLE_S: i64 = 3 * 3600;
/// Fewer traversals than this before a slot and the timetable is used instead of a model.
const MIN_OBS: usize = 50_000;
/// The feed runs about this far behind: the live view for a
/// slot only knows traversals that finished this long before it.
const FEED_LATENCY_S: i64 = 100;

/// One Google option, as stored in data/google/results.csv.
#[derive(Clone, Debug)]
pub struct GoogleRow {
    pub depart: String,
    pub pair: String,
    pub total_s: f64,
    pub transit_s: f64,
    pub bus_only: bool,
    pub lines: String,
}

pub fn load_google(path: &Path) -> Result<Vec<GoogleRow>> {
    let mut out = Vec::new();
    if !path.exists() {
        return Ok(out);
    }
    let mut rd = csv::ReaderBuilder::new().flexible(true).from_path(path)?;
    for r in rd.records() {
        let r = r?;
        if r.len() < 11 {
            continue;
        }
        out.push(GoogleRow {
            depart: r[1].to_string(),
            pair: r[2].to_string(),
            total_s: r[4].parse().unwrap_or(0.0),
            transit_s: r[6].parse().unwrap_or(0.0),
            bus_only: &r[8] == "true",
            lines: r[9].to_string(),
        });
    }
    Ok(out)
}

/// Observed rides: route -> run -> stop position -> (start, seconds).
pub type Runs = HashMap<u32, HashMap<u32, HashMap<u16, (f64, f32)>>>;

pub fn index_runs(rows: &[Row]) -> Runs {
    let mut m: Runs = HashMap::new();
    for r in rows {
        m.entry(r.route).or_default().entry(r.run).or_default().insert(r.k, (r.t_enter, r.obs.dur));
    }
    m
}

/// When a run reached stop position `k`: the start of the hop that leaves
/// it, or failing that the end of the hop that arrives at it.
pub fn reached(run: &HashMap<u16, (f64, f32)>, k: u16) -> Option<f64> {
    run.get(&k).map(|&(t, _)| t).or_else(|| k.checked_sub(1).and_then(|p| run.get(&p)).map(|&(t, d)| t + d as f64))
}

/// Seconds real buses took from stop position `b` to `a` on a route, for
/// buses that set off from `b` in the window starting at `when`.
///
/// Only the two end stops need a trusted time. Hops in between may have been
/// rejected (a GPS gap at an intermediate stop) without affecting the ride
/// time, because hop times telescope; requiring every hop would discard
/// nearly all long rides.
fn observed(runs: &Runs, route: u32, b: u16, a: u16, when: f64) -> Vec<f64> {
    let mut out = Vec::new();
    for run in runs.get(&route).map(|m| m.values()).into_iter().flatten() {
        let (Some(t0), Some(t1)) = (reached(run, b), reached(run, a)) else { continue };
        if t0 < when || t0 - when > OBSERVED_WINDOW_S || t1 <= t0 {
            continue;
        }
        out.push(t1 - t0);
    }
    out
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    v[v.len() / 2]
}

#[derive(Default)]
struct Err {
    n: usize,
    abs: f64,
    signed: f64,
}

impl Err {
    fn add(&mut self, pred: f64, actual: f64) {
        self.n += 1;
        self.abs += (pred - actual).abs();
        self.signed += pred - actual;
    }
    fn show(&self) -> String {
        if self.n == 0 {
            return "no observed rides".into();
        }
        format!("mean abs error {:.1} min, bias {:+.1} min", self.abs / self.n as f64 / 60.0, self.signed / self.n as f64 / 60.0)
    }
}

pub struct Output {
    pub text: String,
    pub csv: String,
}

/// `load_net` reloads the network (each slot gets its own engine).
pub fn run(load_net: &dyn Fn() -> Result<Network>, pairs: &[Pair], google: &[GoogleRow], rows: &[Row]) -> Result<Output> {
    let runs = index_runs(rows);
    // Best bus-only Google option per (pair, departure).
    let mut best: BTreeMap<(String, String), &GoogleRow> = BTreeMap::new();
    for g in google.iter().filter(|g| g.bus_only && g.total_s > 0.0) {
        let e = best.entry((g.depart.clone(), g.pair.clone())).or_insert(g);
        if g.total_s < e.total_s {
            *e = g;
        }
    }
    let mut slots: Vec<String> = best.keys().map(|k| k.0.clone()).collect();
    slots.dedup();

    let mut text = String::new();
    let mut csv = String::from("depart,pair,from,to,kind,basis,ours_door_s,google_door_s,ours_ride_s,google_ride_s,google_lines,leg,route,ride_pred_s,ride_pred_live_s,ride_timetable_s,observed_median_s,observed_n\n");
    let (mut all_model, mut all_live, mut all_tt) = (Err::default(), Err::default(), Err::default());
    for slot in &slots {
        let Some(depart) = Local::parse(slot) else { continue };
        if depart.epoch() + SETTLE_S > crate::clock::now_epoch() {
            let _ = writeln!(text, "\n=== departing {} — skipped: too recent (or in the future) for buses to have completed these rides", &slot[..16]);
            continue;
        }
        let net = load_net()?;
        // Only what was known before this departure time.
        let before: Vec<_> = rows.iter().filter(|r| r.obs.t_enter + (r.obs.dur as i64) < depart.epoch()).map(|r| r.obs).collect();
        let (times, basis): (Arc<dyn SegTimes>, String) = if before.len() >= MIN_OBS {
            (Arc::new(Model::fit(&net, &before)), format!("model fitted on {} earlier traversals", before.len()))
        } else {
            (Arc::new(Scheduled), format!("timetable (only {} traversals before this time)", before.len()))
        };
        // What the server would have known at that moment: the last hour of
        // traversals, less the feed's delay. The option is still chosen on
        // history alone so both columns describe the same bus legs.
        let known = depart.epoch() - FEED_LATENCY_S;
        let recent: Vec<Traversal> = rows
            .iter()
            .filter(|r| {
                let done = r.t_enter + r.obs.dur as f64;
                done <= known as f64 && done >= (known - WINDOW_S) as f64
            })
            .map(|r| Traversal { seg: r.obs.seg, route: r.route, k: r.k, t_enter: r.t_enter, dur: r.obs.dur, gap: 0.0 })
            .collect();
        let blended = Blended { base: times.clone(), live: Arc::new(LiveRatios::compute(&net, times.as_ref(), &recent, known)) };
        let eng = Engine::new(net, times, basis.clone());
        let _ = writeln!(text, "\n=== departing {} — our times: {basis}; live view from {} traversals in the hour before, city at {:.2}x usual", &slot[..16], blended.live.n_obs, blended.live.city_ratio());
        let _ = writeln!(text, "{:<5} {:<30} {:>9} {:>9}   bus legs: route  ours / ours with live / timetable / observed median (n buses)", "pair", "from → to", "ours", "Google");
        let (mut e_model, mut e_live, mut e_tt) = (Err::default(), Err::default(), Err::default());
        for p in pairs {
            let Some(g) = best.get(&(slot.clone(), p.id.clone())) else { continue };
            let from = format!("{:.6},{:.6}", p.from_ll.0, p.from_ll.1);
            let to = format!("{:.6},{:.6}", p.to_ll.0, p.to_ll.1);
            let Ok(resp) = plan(&eng, &from, &to, depart, 1) else { continue };
            let Some(opt) = resp.options.iter().find(|o| o.ride_s > 0) else { continue };
            let mut legs_txt = Vec::new();
            for (i, leg) in opt.legs.iter().enumerate() {
                let LegOut::Bus { route, route_id, from, to, start, duration_s, timetable_s, .. } = leg else { continue };
                let r = &eng.net.routes[eng.net.route_index[route_id] as usize];
                let pos = |sid: &Option<String>, after: usize| {
                    let s = *eng.net.stop_index.get(sid.as_deref()?)?;
                    r.stops.iter().enumerate().skip(after).find(|&(_, &x)| x == s).map(|(i, _)| i)
                };
                let (Some(b), Some(st)) = (pos(&from.stop_id, 0), Local::parse(start)) else { continue };
                let Some(a) = pos(&to.stop_id, b + 1) else { continue };
                // The same leg under the live-corrected times.
                let mut t = st.tod as f32;
                for k in b..a {
                    t += blended.secs(&eng.net, r.segs[k], st.day_type(), (t as u32) % 86_400);
                }
                let live_s = (t - st.tod as f32).round();
                let mut obs = observed(&runs, eng.net.route_index[route_id], b as u16, a as u16, depart.epoch() as f64);
                let (med, n) = if obs.is_empty() { (None, 0) } else { (Some(median(&mut obs)), obs.len()) };
                if let Some(m) = med {
                    e_model.add(*duration_s as f64, m);
                    e_live.add(live_s as f64, m);
                    e_tt.add(*timetable_s as f64, m);
                    all_model.add(*duration_s as f64, m);
                    all_live.add(live_s as f64, m);
                    all_tt.add(*timetable_s as f64, m);
                }
                legs_txt.push(format!(
                    "{route} {}/{:.0}/{}/{}",
                    duration_s / 60,
                    live_s / 60.0,
                    timetable_s / 60,
                    med.map_or("–".to_string(), |m| format!("{:.0} ({n})", m / 60.0))
                ));
                let _ = writeln!(
                    csv,
                    "{slot},{},{},{},{},{},{},{:.0},{},{:.0},{},{i},{route},{duration_s},{live_s:.0},{timetable_s},{},{n}",
                    p.id,
                    p.from,
                    p.to,
                    p.kind,
                    eng.times.name(),
                    opt.duration_s,
                    g.total_s,
                    opt.ride_s,
                    g.transit_s,
                    g.lines,
                    med.map_or(String::new(), |m| format!("{m:.0}"))
                );
            }
            let _ = writeln!(
                text,
                "{:<5} {:<30} {:>5} min {:>5} min   {}",
                p.id,
                format!("{} → {}", &p.from[..p.from.len().min(13)], &p.to[..p.to.len().min(13)]),
                opt.duration_s / 60,
                (g.total_s / 60.0).round(),
                legs_txt.join("; ")
            );
        }
        let _ = writeln!(text, "  bus legs with real buses to compare against: {} — ours: {}; ours with live: {}; timetable: {}", e_model.n, e_model.show(), e_live.show(), e_tt.show());
    }
    let _ = writeln!(text, "\nAll slots: {} bus legs with observed rides — ours: {}; ours with live: {}; timetable: {}", all_model.n, all_model.show(), all_live.show(), all_tt.show());
    let _ = writeln!(text, "\"Google\" is its best bus-only option door to door; it is usually a different set of buses from ours, so it is shown for context, not scored against observed rides.");
    Ok(Output { text, csv })
}

/// Do buses leave their first stop when the timetable says they do?
///
/// Uses traversals of each route's first segment (their start is the moment
/// the bus left the terminal). For every observed departure, the gap to the
/// nearest timetabled start of that route is compared with the gap you would
/// expect if departures had nothing to do with the timetable (a quarter of
/// the timetable's spacing at that time).
pub fn departures(net: &Network, rows: &[Row]) -> String {
    let mut text = String::new();
    // One group per route per day, so rates are per day of service.
    let mut by_route: HashMap<(u32, i64), Vec<f64>> = HashMap::new();
    for r in rows.iter().filter(|r| r.k == 0) {
        by_route.entry((r.route, Local::from_epoch(r.t_enter as i64).day)).or_default().push(r.t_enter);
    }
    let (mut offs, mut expect, mut within2, mut within5, mut n) = (Vec::new(), Vec::new(), 0usize, 0usize, 0usize);
    let (mut obs_per_hr, mut sched_per_hr) = (Vec::new(), Vec::new());
    for (&(ri, _), deps) in &by_route {
        let starts = net.starts_through(ri, 0);
        if starts.len() < 2 {
            continue;
        }
        for &t in deps {
            let tod = Local::from_epoch(t as i64).tod as f64 + t.fract();
            let i = starts.partition_point(|&s| (s as f64) < tod);
            let prev = if i > 0 { Some(starts[i - 1] as f64) } else { None };
            let next = starts.get(i).map(|&s| s as f64);
            let off = [prev, next].iter().flatten().map(|s| (s - tod).abs()).fold(f64::MAX, f64::min);
            if let (Some(p), Some(nx)) = (prev, next) {
                expect.push((nx - p) / 4.0);
            }
            offs.push(off);
            within2 += (off <= 120.0) as usize;
            within5 += (off <= 300.0) as usize;
            n += 1;
        }
        // Departures per hour over the span this route was actually seen.
        let (lo, hi) = (deps.iter().copied().fold(f64::MAX, f64::min), deps.iter().copied().fold(f64::MIN, f64::max));
        if deps.len() >= 4 && hi - lo > 1800.0 {
            let (a, b) = (Local::from_epoch(lo as i64).tod, Local::from_epoch(hi as i64).tod);
            if b > a {
                let sched = starts.iter().filter(|&&s| s >= a && s <= b).count();
                let hrs = (hi - lo) / 3600.0;
                obs_per_hr.push((deps.len() - 1) as f64 / hrs);
                sched_per_hr.push(sched.saturating_sub(1) as f64 / hrs);
            }
        }
    }
    if n == 0 {
        return "no observed terminal departures yet\n".into();
    }
    let routes: std::collections::HashSet<u32> = by_route.keys().map(|k| k.0).collect();
    let _ = writeln!(text, "{n} observed terminal departures on {} routes", routes.len());
    let _ = writeln!(text, "gap to the nearest timetabled start: median {:.1} min; within 2 min: {:.0}%; within 5 min: {:.0}%", median(&mut offs) / 60.0, 100.0 * within2 as f64 / n as f64, 100.0 * within5 as f64 / n as f64);
    if !expect.is_empty() {
        let _ = writeln!(text, "median gap expected if departures ignored the timetable: {:.1} min", median(&mut expect) / 60.0);
    }
    if !obs_per_hr.is_empty() {
        let _ = writeln!(
            text,
            "on {} route-days seen for 30+ min: departures per hour — observed median {:.1}, timetabled median {:.1}",
            obs_per_hr.len(),
            median(&mut obs_per_hr),
            median(&mut sched_per_hr)
        );
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ride_is_matched_by_its_end_stops_even_with_a_hop_missing() {
        // One run on route R: hops 0, 1 and 3 were timed, hop 2 was rejected.
        let mut run = HashMap::new();
        run.insert(0u16, (1000.0, 60.0f32));
        run.insert(1, (1060.0, 50.0));
        run.insert(3, (1200.0, 40.0));
        let mut runs: Runs = HashMap::new();
        runs.entry(7).or_default().insert(0, run);
        // Stop 0 to stop 4: starts at 1000, hop 3 ends at 1240.
        assert_eq!(observed(&runs, 7, 0, 4, 900.0), vec![240.0]);
        // Stop 2 is reached at the end of hop 1 (1110); stop 3 at the start of hop 3.
        assert_eq!(observed(&runs, 7, 2, 3, 900.0), vec![90.0]);
        // Boarded before the window opens: not counted.
        assert!(observed(&runs, 7, 0, 4, 1001.0).is_empty());
        // No trusted time at stop 5.
        assert!(observed(&runs, 7, 0, 5, 900.0).is_empty());
    }
}
