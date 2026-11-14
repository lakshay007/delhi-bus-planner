//! Numbers for the validation report (VALIDATION_REPORT.md): our
//! estimates, Google's and the timetable's, each against what buses did, on
//! the same bus legs.
//!
//! A "leg" here is one bus ride Google proposed: a line, a boarding stop and
//! an alighting stop. Each is matched to the same route and stops in our
//! network, so every estimate is for the same ride and can be scored against
//! the real buses that made it.
//!
//! Two comparisons are produced:
//!
//!   same day    the slots Google was asked about, on days we have GPS for.
//!               The model is fitted only on data from before each slot and
//!               scored on buses that boarded in the 90 minutes after it.
//!   unseen week Google cannot be asked about the past, but its bus durations
//!               do not change with the hour or the day (shown in the
//!               output). So the duration Google gave for a leg is also
//!               scored against every ride of that leg in a held-out week of
//!               the archive, next to a model fitted on the weeks before.

use crate::clock::Local;
use crate::compare::{Runs, index_runs, reached};
use crate::eval::{Acc, Row, line};
use crate::geom::to_xy;
use crate::google::Pair;
use crate::gtfs::{Network, Route};
use crate::link::Traversal;
use crate::live::{Blended, LiveRatios, WINDOW_S};
use crate::model::{Model, Obs};
use crate::raptor::{Scheduled, SegTimes};
use anyhow::{Context, Result};
use rayon::prelude::*;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;

/// A Google stop further than this from every stop of the route is not a match.
const MATCH_M: f64 = 250.0;
/// Same-day scoring: buses that boarded within this long after the slot.
const AFTER_SLOT_S: f64 = 5400.0;
/// A slot is scored once this much time has passed since it.
const SETTLE_S: i64 = 3 * 3600;
/// The feed's delay: a live view at time T knows traversals finished by T - this.
const FEED_LATENCY_S: i64 = 100;
/// Live views in the unseen-week replay are rebuilt on this grid.
const STEP_S: i64 = 300;
/// A ride slower than this end to end is not a ride (the per-hop rule of link.rs).
const MIN_SPEED: f64 = 0.4;

/// One bus leg from a Google response.
#[derive(Clone, Debug)]
pub struct GLeg {
    pub slot: String,
    pub pair: String,
    pub option: usize,
    pub line: String,
    pub from: (f64, f64),
    pub to: (f64, f64),
    pub dur_s: f32,
    /// About the option this leg belongs to: vehicle legs in it, whether all
    /// of them are DTC buses, and Google's door-to-door seconds.
    pub opt_legs: usize,
    pub opt_all_bus: bool,
    pub opt_total_s: f32,
}

/// The same leg in our network.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Leg {
    pub route: u32,
    pub b: u16,
    pub a: u16,
}

fn secs(v: &Value) -> f32 {
    v.as_str().and_then(|s| s.trim_end_matches('s').parse().ok()).unwrap_or(0.0)
}

/// Every DTC bus leg in the stored Google responses for our pairs. Slots
/// listed in `control_slots.txt` (queries about other cities) are skipped.
pub fn load_google_legs(google_dir: &Path, pairs: &[Pair]) -> Result<Vec<GLeg>> {
    let controls: HashSet<String> =
        std::fs::read_to_string(google_dir.join("control_slots.txt")).unwrap_or_default().split_whitespace().map(|s| s.replace(':', "")).collect();
    let raw = google_dir.join("raw");
    let mut slots: Vec<String> = std::fs::read_dir(&raw)
        .with_context(|| format!("reading {}", raw.display()))?
        .filter_map(|e| e.ok().and_then(|e| e.file_name().into_string().ok()))
        .filter(|s| s.len() == 15 && !controls.contains(s))
        .collect();
    slots.sort();
    let mut out = Vec::new();
    for slot in slots {
        for p in pairs {
            let Ok(text) = std::fs::read_to_string(raw.join(&slot).join(format!("{}.json", p.id))) else { continue };
            let Ok(resp) = serde_json::from_str::<Value>(&text) else { continue };
            for (option, r) in resp["routes"].as_array().map(|v| v.as_slice()).unwrap_or(&[]).iter().enumerate() {
                let steps = || r["legs"].as_array().into_iter().flatten().flat_map(|l| l["steps"].as_array().into_iter().flatten());
                let is_bus = |st: &Value| st["transitDetails"]["transitLine"]["vehicle"]["type"] == "BUS" && st["transitDetails"]["transitLine"]["agencies"][0]["name"] == "DTC";
                let opt_legs = steps().filter(|st| st["travelMode"] == "TRANSIT").count();
                let opt_all_bus = steps().filter(|st| st["travelMode"] == "TRANSIT").all(is_bus);
                let opt_total_s = secs(&r["duration"]);
                for st in steps() {
                    let td = &st["transitDetails"];
                    let tl = &td["transitLine"];
                    if st["travelMode"] != "TRANSIT" || !is_bus(st) {
                        continue;
                    }
                    let sd = &td["stopDetails"];
                    let ll = |k: &str| (sd[k]["location"]["latLng"]["latitude"].as_f64(), sd[k]["location"]["latLng"]["longitude"].as_f64());
                    let ((Some(a), Some(b)), (Some(c), Some(d))) = (ll("departureStop"), ll("arrivalStop")) else { continue };
                    // Only the difference of the two times is used, so their time zone does not matter.
                    let when = |k: &str| sd[k].as_str().and_then(Local::parse).map(|l| l.epoch());
                    let dur_s = match (when("departureTime"), when("arrivalTime")) {
                        (Some(x), Some(y)) if y > x => (y - x) as f32,
                        _ => secs(&st["staticDuration"]),
                    };
                    out.push(GLeg {
                        slot: format!("{}:{}", &slot[..13], &slot[13..]),
                        pair: p.id.clone(),
                        option,
                        line: tl["nameShort"].as_str().or(tl["name"].as_str()).unwrap_or("?").to_string(),
                        from: (a, b),
                        to: (c, d),
                        dur_s,
                        opt_legs,
                        opt_all_bus,
                        opt_total_s,
                    });
                }
            }
        }
    }
    Ok(out)
}

/// A line name reduced to what both sources agree on: "Airport Exp.-4" and
/// "AIRPORT-EXP-04" both become "AIRPORTEXP4".
fn norm(name: &str) -> String {
    let up: Vec<char> = name.to_uppercase().chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    let mut out = String::new();
    for (i, &c) in up.iter().enumerate() {
        // Drop zeros that lead a number.
        let leads = c == '0' && !out.chars().last().is_some_and(|p| p.is_ascii_digit()) && up.get(i + 1).is_some_and(|n| n.is_ascii_digit());
        if !leads {
            out.push(c);
        }
    }
    out.replace("EXTN", "EXT")
}

/// Route name without its direction suffix.
fn base(name: &str) -> &str {
    name.strip_suffix("DOWN").or_else(|| name.strip_suffix("UP")).unwrap_or(name)
}

/// Finds the route and stop positions a Google leg corresponds to: a route
/// with the same line name, and on it the boarding and alighting stops (in
/// that order) nearest to Google's.
pub struct Matcher<'a> {
    net: &'a Network,
    by_name: HashMap<String, Vec<u32>>,
}

impl<'a> Matcher<'a> {
    pub fn new(net: &'a Network) -> Self {
        let mut by_name: HashMap<String, Vec<u32>> = HashMap::new();
        for (i, r) in net.routes.iter().enumerate() {
            by_name.entry(norm(base(&r.name))).or_default().push(i as u32);
        }
        Matcher { net, by_name }
    }

    /// The match and the larger of the two stop distances, metres.
    pub fn find(&self, g: &GLeg) -> Option<(Leg, f64)> {
        let (from, to) = (to_xy(g.from.0, g.from.1), to_xy(g.to.0, g.to.1));
        let dist = |s: u32, p: (f64, f64)| {
            let q = self.net.stops[s as usize].xy;
            ((q.0 - p.0).powi(2) + (q.1 - p.1).powi(2)).sqrt()
        };
        let mut best: Option<(Leg, f64)> = None;
        for &ri in self.by_name.get(&norm(&g.line))? {
            let r = &self.net.routes[ri as usize];
            // Best boarding stop at or before each position, then the best alighting stop after it.
            let mut board: Option<(usize, f64)> = None;
            for k in 0..r.stops.len() {
                if let Some((b, db)) = board {
                    let da = dist(r.stops[k], to);
                    let worst = db.max(da);
                    if worst <= MATCH_M && best.is_none_or(|x| worst < x.1) {
                        best = Some((Leg { route: ri, b: b as u16, a: k as u16 }, worst));
                    }
                }
                let d = dist(r.stops[k], from);
                if board.is_none_or(|x| d < x.1) {
                    board = Some((k, d));
                }
            }
        }
        best
    }
}

/// Seconds for a leg that starts at `start`, each hop timed when the bus gets there.
fn leg_secs(net: &Network, times: &dyn SegTimes, leg: Leg, start: Local) -> f32 {
    let r: &Route = &net.routes[leg.route as usize];
    let dt = start.day_type();
    let mut t = start.tod as f32;
    for k in leg.b as usize..leg.a as usize {
        t += times.secs(net, r.segs[k], dt, (t as u32) % 86_400);
    }
    t - start.tod as f32
}

fn leg_metres(net: &Network, leg: Leg) -> f64 {
    let r = &net.routes[leg.route as usize];
    r.along[leg.a as usize] - r.along[leg.b as usize]
}

/// Observed rides of a leg that boarded in [from, until): (boarding time, seconds).
fn rides(net: &Network, runs: &Runs, leg: Leg, from: f64, until: f64) -> Vec<(f64, f32)> {
    let longest = leg_metres(net, leg) / MIN_SPEED;
    let mut out = Vec::new();
    for run in runs.get(&leg.route).map(|m| m.values()).into_iter().flatten() {
        let (Some(t0), Some(t1)) = (reached(run, leg.b), reached(run, leg.a)) else { continue };
        if t0 >= from && t0 < until && t1 > t0 && t1 - t0 <= longest {
            out.push((t0, (t1 - t0) as f32));
        }
    }
    out
}

fn median(v: &mut [f32]) -> f32 {
    v.sort_by(|a, b| a.total_cmp(b));
    v[v.len() / 2]
}

const NAMES: [&str; 4] = ["Google Maps", "timetable (18 km/h)", "our model, history only", "our model + live"];

pub struct Output {
    pub text: String,
    /// One row per Google leg per slot (same-day comparison).
    pub legs_csv: String,
    /// Error by hour of day (unseen week).
    pub hours_csv: String,
    /// One row per origin-destination pair and part of the day (unseen week, Monday to Friday).
    pub pairs_csv: String,
}

/// Parts of the day in the per-pair table: label, first hour, hour after the last.
const BANDS: [(&str, u32, u32); 4] = [("08-10", 8, 10), ("12-14", 12, 14), ("17-19", 17, 19), ("21-23", 21, 23)];

/// `fit_until`..`test_until` is the unseen week: the model for it is fitted on rows before `fit_until`.
pub fn run(net: &Network, rows: &[Row], pairs: &[Pair], google_dir: &Path, fit_until: i64, test_until: i64) -> Result<Output> {
    let mut text = String::new();
    let glegs = load_google_legs(google_dir, pairs)?;
    let matcher = Matcher::new(net);
    let matched: Vec<Option<(Leg, f64)>> = glegs.iter().map(|g| matcher.find(g)).collect();
    let n_matched = matched.iter().flatten().count();
    let name_known = glegs.iter().filter(|g| matcher.by_name.contains_key(&norm(&g.line))).count();
    let mut miss: Vec<f32> = matched.iter().flatten().map(|m| m.1 as f32).collect();
    let _ = writeln!(text, "Google bus legs in the stored responses: {}", glegs.len());
    let _ = writeln!(text, "  line name found in our network: {} ({:.0}%)", name_known, 100.0 * name_known as f64 / glegs.len().max(1) as f64);
    let _ = writeln!(
        text,
        "  matched to a route and a pair of stops within {MATCH_M:.0} m: {} ({:.0}%); median stop distance {:.0} m",
        n_matched,
        100.0 * n_matched as f64 / glegs.len().max(1) as f64,
        if miss.is_empty() { 0.0 } else { median(&mut miss) }
    );

    // ---- Google's duration for a leg: does it change between slots?
    let mut durs: BTreeMap<Leg, Vec<(String, f32)>> = BTreeMap::new();
    for (g, m) in glegs.iter().zip(&matched) {
        if let Some((leg, _)) = m {
            let v = durs.entry(*leg).or_default();
            if !v.iter().any(|x| x.0 == g.slot) {
                v.push((g.slot.clone(), g.dur_s));
            }
        }
    }
    let multi: Vec<&Vec<(String, f32)>> = durs.values().filter(|v| v.len() >= 2).collect();
    let same = multi.iter().filter(|v| v.iter().all(|x| (x.1 - v[0].1).abs() <= 60.0)).count();
    let mut ratio: Vec<f32> = durs
        .iter()
        .map(|(leg, v)| {
            let mut d: Vec<f32> = v.iter().map(|x| x.1).collect();
            median(&mut d) / leg_secs(net, &Scheduled, *leg, Local { day: 0, tod: 43_200 })
        })
        .collect();
    let _ = writeln!(text, "distinct legs (route, boarding stop, alighting stop): {}", durs.len());
    let _ = writeln!(
        text,
        "  asked about at two or more times of day: {}; Google's duration the same to within a minute at every time: {} ({:.0}%)",
        multi.len(),
        same,
        100.0 * same as f64 / multi.len().max(1) as f64
    );
    if !ratio.is_empty() {
        let _ = writeln!(text, "  Google's duration / our timetable's for the same leg: median {:.2}", median(&mut ratio));
    }

    let runs = index_runs(rows);
    let sched = Scheduled;

    // ---- 1. same day
    let _ = writeln!(text, "\n== Same day: each slot Google was asked about, scored on buses that boarded in the {:.0} min after it", AFTER_SLOT_S / 60.0);
    let mut legs_csv = String::from("slot,pair,option,line,route,from_stop,to_stop,stops,metres,google_s,timetable_s,ours_s,ours_live_s,observed_median_s,observed_n\n");
    let mut slots: Vec<String> = glegs.iter().map(|g| g.slot.clone()).collect();
    slots.sort();
    slots.dedup();
    let data_end = rows.iter().map(|r| r.obs.t_enter).max().unwrap_or(0);
    let mut all: [Acc; 4] = Default::default();
    let mut all_legs = 0usize;
    for slot in &slots {
        let Some(when) = Local::parse(slot) else { continue };
        if when.epoch() + SETTLE_S > data_end {
            let _ = writeln!(text, "{slot}: skipped, the archive does not yet reach {} h past it", SETTLE_S / 3600);
            continue;
        }
        // What was known at the slot: a model from everything finished before it, and the last hour of buses.
        let before: Vec<Obs> = rows.iter().filter(|r| r.obs.t_enter + (r.obs.dur as i64) < when.epoch()).map(|r| r.obs).collect();
        let model: Arc<dyn SegTimes> = Arc::new(Model::fit(net, &before));
        let known = when.epoch() - FEED_LATENCY_S;
        let recent: Vec<Traversal> = rows
            .iter()
            .filter(|r| {
                let done = r.t_enter + r.obs.dur as f64;
                done <= known as f64 && done >= (known - WINDOW_S) as f64
            })
            .map(|r| Traversal { seg: r.obs.seg, route: r.route, k: r.k, t_enter: r.t_enter, dur: r.obs.dur, gap: 0.0 })
            .collect();
        let blended = Blended { base: model.clone(), live: Arc::new(LiveRatios::compute(net, model.as_ref(), &recent, known)) };
        let mut acc: [Acc; 4] = Default::default();
        let mut seen: HashSet<Leg> = HashSet::new();
        let (mut n_legs, mut n_no_bus) = (0usize, 0usize);
        for (g, m) in glegs.iter().zip(&matched).filter(|x| &x.0.slot == slot) {
            let Some((leg, _)) = *m else { continue };
            let r = &net.routes[leg.route as usize];
            let preds = [g.dur_s, leg_secs(net, &sched, leg, when), leg_secs(net, model.as_ref(), leg, when), leg_secs(net, &blended, leg, when)];
            let obs = rides(net, &runs, leg, when.epoch() as f64, when.epoch() as f64 + AFTER_SLOT_S);
            let mut d: Vec<f32> = obs.iter().map(|x| x.1).collect();
            let _ = writeln!(
                legs_csv,
                "{slot},{},{},{},{},{},{},{},{:.0},{:.0},{:.0},{:.0},{:.0},{},{}",
                g.pair,
                g.option,
                g.line,
                r.name,
                net.stops[r.stops[leg.b as usize] as usize].name.replace(',', " "),
                net.stops[r.stops[leg.a as usize] as usize].name.replace(',', " "),
                leg.a - leg.b,
                leg_metres(net, leg),
                preds[0],
                preds[1],
                preds[2],
                preds[3],
                if d.is_empty() { String::new() } else { format!("{:.0}", median(&mut d)) },
                d.len()
            );
            // Score each distinct leg once per slot, against every bus that made it.
            if !seen.insert(leg) {
                continue;
            }
            if obs.is_empty() {
                n_no_bus += 1;
                continue;
            }
            n_legs += 1;
            for &(_, actual) in &obs {
                for p in 0..4 {
                    acc[p].push(preds[p], actual);
                    all[p].push(preds[p], actual);
                }
            }
        }
        all_legs += n_legs;
        let _ = writeln!(text, "{slot}: {n_legs} distinct legs with observed rides ({n_no_bus} had none in the window), mean actual ride {:.1} min; minutes", acc[0].mean_actual() / 60.0);
        for p in 0..4 {
            let _ = writeln!(text, "{}", line(NAMES[p], &acc[p], 60.0, "m"));
        }
    }
    let _ = writeln!(text, "all scored slots: {all_legs} leg-slots, mean actual ride {:.1} min; minutes", all[0].mean_actual() / 60.0);
    for p in 0..4 {
        let _ = writeln!(text, "{}", line(NAMES[p], &all[p], 60.0, "m"));
    }

    // ---- 2. unseen week
    let _ = writeln!(
        text,
        "\n== Unseen week: {} to {}. Model fitted on everything before it; Google's duration for each leg is the one it gave in November",
        Local::from_epoch(fit_until).date_string(),
        Local::from_epoch(test_until - 1).date_string()
    );
    let train: Vec<Obs> = rows.iter().filter(|r| r.obs.t_enter < fit_until).map(|r| r.obs).collect();
    let model = Arc::new(Model::fit(net, &train));
    // (leg, Google's seconds, boarding time, actual seconds)
    let mut all_rides: Vec<(Leg, f32, f64, f32)> = Vec::new();
    for (leg, v) in &durs {
        let mut d: Vec<f32> = v.iter().map(|x| x.1).collect();
        let g = median(&mut d);
        all_rides.extend(rides(net, &runs, *leg, fit_until as f64, test_until as f64).into_iter().map(|(t0, a)| (*leg, g, t0, a)));
    }
    all_rides.sort_by(|a, b| a.2.total_cmp(&b.2));
    let legs_with_rides: HashSet<Leg> = all_rides.iter().map(|x| x.0).collect();
    // Traversals a live view in the test period can know: (finished, segment, observed / usual).
    let mut known: Vec<(f64, u32, f32)> = rows
        .par_iter()
        .filter(|r| r.obs.t_enter >= fit_until - WINDOW_S - 900 && r.obs.t_enter < test_until && net.segments[r.obs.seg as usize].len_m >= 30.0)
        .map(|r| {
            let at = Local::from_epoch(r.obs.t_enter);
            (r.t_enter + r.obs.dur as f64, r.obs.seg, r.obs.dur / model.secs(net, r.obs.seg, at.day_type(), at.tod))
        })
        .collect();
    known.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut groups: Vec<(i64, usize, usize)> = Vec::new();
    let mut i = 0;
    while i < all_rides.len() {
        let slot = (all_rides[i].2 as i64).div_euclid(STEP_S) * STEP_S;
        let mut j = i;
        while j < all_rides.len() && (all_rides[j].2 as i64) < slot + STEP_S {
            j += 1;
        }
        groups.push((slot, i, j));
        i = j;
    }
    // Per ride: the four predictions, the actual time, the hour, the day and the leg.
    let scored: Vec<([f32; 4], f32, u32, i64, Leg)> = groups
        .par_iter()
        .flat_map_iter(|&(slot, i, j)| {
            let until = slot - FEED_LATENCY_S;
            let lo = known.partition_point(|x| x.0 < (until - WINDOW_S) as f64);
            let hi = known.partition_point(|x| x.0 <= until as f64);
            let items = known[lo..hi].iter().map(|&(done, seg, r)| (seg, (until as f64 - done) as f32, r));
            let blended = Blended { base: model.clone(), live: Arc::new(LiveRatios::from_ratios(net, items, until, &Default::default())) };
            all_rides[i..j]
                .iter()
                .map(|&(leg, g, t0, actual)| {
                    let at = Local::from_epoch(t0 as i64);
                    ([g, leg_secs(net, &sched, leg, at), leg_secs(net, model.as_ref(), leg, at), leg_secs(net, &blended, leg, at)], actual, at.tod / 3600, at.day, leg)
                })
                .collect::<Vec<_>>()
        })
        .collect();
    let mut acc: [Acc; 4] = Default::default();
    let mut by_hour: Vec<[Acc; 4]> = (0..24).map(|_| Default::default()).collect();
    let mut by_day: BTreeMap<i64, [Acc; 4]> = BTreeMap::new();
    for (preds, actual, h, day, _) in &scored {
        let d = by_day.entry(*day).or_default();
        for p in 0..4 {
            acc[p].push(preds[p], *actual);
            by_hour[*h as usize][p].push(preds[p], *actual);
            d[p].push(preds[p], *actual);
        }
    }
    let mean_min = scored.iter().map(|x| x.1 as f64).sum::<f64>() / scored.len().max(1) as f64 / 60.0;
    let _ = writeln!(text, "{} rides on {} of the {} distinct legs, mean ride {:.1} min; minutes", scored.len(), legs_with_rides.len(), durs.len(), mean_min);
    for p in 0..4 {
        let _ = writeln!(text, "{}", line(NAMES[p], &acc[p], 60.0, "m"));
    }
    let cell = |a: &Acc| a.stats().map_or(String::new(), |s| format!("{:5.1} ({:>+5.1})", s.1 / 60.0, s.5 / 60.0));
    let mut hours_csv = String::from("hour,rides,mean_actual_min,google_mae,google_bias,timetable_mae,timetable_bias,model_mae,model_bias,model_live_mae,model_live_bias,google_mape,timetable_mape,model_mape,model_live_mape\n");
    let _ = writeln!(text, "\nby boarding hour: MAE minutes (bias)\n  hour   rides   Google           timetable        model            model + live");
    for (h, a) in by_hour.iter().enumerate() {
        let Some(s) = a[0].stats() else { continue };
        let _ = writeln!(text, "  {h:02}:00 {:>6}   {}    {}    {}    {}", s.0, cell(&a[0]), cell(&a[1]), cell(&a[2]), cell(&a[3]));
        let st: Vec<(usize, f32, f32, f32, f32, f32)> = a.iter().map(|x| x.stats().unwrap_or_default()).collect();
        let actual = a[0].mean_actual() / 60.0;
        let _ = writeln!(
            hours_csv,
            "{h},{},{actual:.2},{:.2},{:.2},{:.2},{:.2},{:.2},{:.2},{:.2},{:.2},{:.1},{:.1},{:.1},{:.1}",
            s.0,
            st[0].1 / 60.0,
            st[0].5 / 60.0,
            st[1].1 / 60.0,
            st[1].5 / 60.0,
            st[2].1 / 60.0,
            st[2].5 / 60.0,
            st[3].1 / 60.0,
            st[3].5 / 60.0,
            st[0].2,
            st[1].2,
            st[2].2,
            st[3].2
        );
    }
    let _ = writeln!(text, "\nby day: MAE minutes (bias)\n  date        kind       rides   Google           timetable        model            model + live");
    for (day, a) in &by_day {
        let l = Local { day: *day, tod: 0 };
        let _ = writeln!(
            text,
            "  {}  {:<9} {:>6}   {}    {}    {}    {}",
            l.date_string(),
            crate::clock::DAY_KIND_NAMES[l.day_type()],
            a[0].stats().map_or(0, |s| s.0),
            cell(&a[0]),
            cell(&a[1]),
            cell(&a[2]),
            cell(&a[3])
        );
    }
    // ---- 3. per origin-destination pair, by part of the day (Monday to Friday of the unseen week)
    let mut options: BTreeMap<(String, String, usize), Vec<(&GLeg, Option<Leg>)>> = BTreeMap::new();
    for (g, m) in glegs.iter().zip(&matched) {
        options.entry((g.pair.clone(), g.slot.clone(), g.option)).or_default().push((g, m.map(|x| x.0)));
    }
    // (leg, band) -> actual, model and model + live seconds of every weekday ride.
    let mut cells: HashMap<(Leg, usize), [Vec<f32>; 3]> = HashMap::new();
    for (preds, actual, h, day, leg) in &scored {
        if (Local { day: *day, tod: 0 }).day_type() != 0 {
            continue;
        }
        if let Some(bi) = BANDS.iter().position(|b| *h >= b.1 && *h < b.2) {
            let c = cells.entry((*leg, bi)).or_default();
            c[0].push(*actual);
            c[1].push(preds[2]);
            c[2].push(preds[3]);
        }
    }
    // Among a pair's all-bus options with every leg matched, take the one
    // that can be checked at the most times of day (every leg has at least 3
    // observed rides in the band); ties go to the one Google says is fastest.
    let mut chosen: BTreeMap<String, (usize, Vec<(&GLeg, Leg)>)> = BTreeMap::new();
    for ((pair, _, _), legs) in &options {
        let g0 = legs[0].0;
        if !g0.opt_all_bus || legs.len() != g0.opt_legs || legs.iter().any(|l| l.1.is_none()) {
            continue;
        }
        let cover = (0..BANDS.len()).filter(|&bi| legs.iter().all(|l| cells.get(&(l.1.unwrap(), bi)).is_some_and(|c| c[0].len() >= 3))).count();
        let better = chosen.get(pair).is_none_or(|c| cover > c.0 || (cover == c.0 && g0.opt_total_s < c.1[0].0.opt_total_s));
        if better {
            chosen.insert(pair.clone(), (cover, legs.iter().map(|l| (l.0, l.1.unwrap())).collect()));
        }
    }
    let chosen: BTreeMap<String, Vec<(&GLeg, Leg)>> = chosen.into_iter().map(|(k, v)| (k, v.1)).collect();
    let mut pairs_csv = String::from("pair,from,to,lines,legs,metres,google_ride_s,timetable_ride_s,band,observed_median_s,observed_n,ours_s,ours_live_s\n");
    let mut pair_acc: [Acc; 4] = Default::default();
    for p in pairs {
        let Some(legs) = chosen.get(&p.id) else { continue };
        let lines: Vec<&str> = legs.iter().map(|l| net.routes[l.1.route as usize].name.as_str()).collect();
        let google: f32 = legs.iter().map(|l| l.0.dur_s).sum();
        let tt: f32 = legs.iter().map(|l| leg_secs(net, &sched, l.1, Local { day: 0, tod: 43_200 })).sum();
        let metres: f64 = legs.iter().map(|l| leg_metres(net, l.1)).sum();
        for (bi, band) in BANDS.iter().enumerate() {
            // The journey's ride time is the sum over its legs; every leg needs at least 3 observed rides.
            let mut sums = [0f32; 3];
            let mut min_n = usize::MAX;
            for l in legs {
                match cells.get_mut(&(l.1, bi)) {
                    Some(c) if c[0].len() >= 3 => {
                        min_n = min_n.min(c[0].len());
                        for i in 0..3 {
                            sums[i] += median(&mut c[i]);
                        }
                    }
                    _ => min_n = 0,
                }
            }
            if min_n == 0 || min_n == usize::MAX {
                let _ = writeln!(pairs_csv, "{},{},{},{},{},{metres:.0},{google:.0},{tt:.0},{},,0,,", p.id, p.from.replace(',', " "), p.to.replace(',', " "), lines.join("|"), legs.len(), band.0);
                continue;
            }
            let _ = writeln!(
                pairs_csv,
                "{},{},{},{},{},{metres:.0},{google:.0},{tt:.0},{},{:.0},{min_n},{:.0},{:.0}",
                p.id,
                p.from.replace(',', " "),
                p.to.replace(',', " "),
                lines.join("|"),
                legs.len(),
                band.0,
                sums[0],
                sums[1],
                sums[2]
            );
            for (i, pred) in [google, tt, sums[1], sums[2]].into_iter().enumerate() {
                pair_acc[i].push(pred, sums[0]);
            }
        }
    }
    let _ = writeln!(
        text,
        "\n== Per origin-destination pair (Monday to Friday of the unseen week): one of Google's all-bus options, ride time summed over its legs, against the median real ride in four parts of the day\n{} of {} pairs have such an option with every leg matched; pair-and-time cells with observed rides on every leg: {}, mean actual ride {:.1} min; minutes",
        chosen.len(),
        pairs.len(),
        pair_acc[0].stats().map_or(0, |s| s.0),
        pair_acc[0].mean_actual() / 60.0
    );
    for p in 0..4 {
        let _ = writeln!(text, "{}", line(NAMES[p], &pair_acc[p], 60.0, "m"));
    }
    Ok(Output { text, legs_csv, hours_csv, pairs_csv })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::Polyline;
    use crate::gtfs::{Segment, Stop};

    /// Route "405UP": four stops about 490 m apart, heading east.
    fn toy() -> Network {
        let stops: Vec<Stop> = (0..4)
            .map(|i| {
                let (lat, lon) = (28.6, 77.2 + i as f64 * 0.005);
                Stop { id: format!("s{i}"), name: String::new(), lat, lon, xy: to_xy(lat, lon) }
            })
            .collect();
        let xs: Vec<(f64, f64)> = stops.iter().map(|s| s.xy).collect();
        let along: Vec<f64> = xs.iter().map(|p| p.0 - xs[0].0).collect();
        let route = Route {
            id: "r".into(),
            name: "405UP".into(),
            desc: String::new(),
            agency: String::new(),
            stops: (0..4).collect(),
            sched: vec![0, 100, 200, 300],
            starts: vec![0],
            shape: Polyline::new(xs),
            along,
            segs: vec![0, 1, 2],
        };
        Network {
            stops,
            routes: vec![route],
            segments: (0..3).map(|i| Segment { a: i, b: i + 1, len_m: 490.0, sched_s: 100.0, n_routes: 1 }).collect(),
            stop_index: HashMap::new(),
            route_index: HashMap::new(),
            seg_index: HashMap::new(),
            stop_routes: vec![],
            footpaths: vec![],
            seg_neighbours: vec![vec![1], vec![0, 2], vec![1]],
            siblings: vec![],
        }
    }

    fn gleg(line: &str, from: (f64, f64), to: (f64, f64)) -> GLeg {
        GLeg { slot: String::new(), pair: String::new(), option: 0, line: line.into(), from, to, dur_s: 0.0, opt_legs: 1, opt_all_bus: true, opt_total_s: 0.0 }
    }

    #[test]
    fn a_google_leg_is_matched_by_line_name_and_stop_order() {
        let net = toy();
        let m = Matcher::new(&net);
        // Google's stops sit a few metres from our stops 1 and 3.
        let (near1, near3) = ((28.6002, 77.205), (28.6001, 77.215));
        assert_eq!(m.find(&gleg("405", near1, near3)).map(|x| x.0), Some(Leg { route: 0, b: 1, a: 3 }));
        // The same stops in the other order are the opposite direction: no match on this route.
        assert!(m.find(&gleg("405", near3, near1)).is_none());
        // A different line, or a stop too far from the route.
        assert!(m.find(&gleg("406", near1, near3)).is_none());
        assert!(m.find(&gleg("405", near1, (28.61, 77.215))).is_none());
        // The timetable gives 100 s per hop.
        assert_eq!(leg_secs(&net, &Scheduled, Leg { route: 0, b: 1, a: 3 }, Local { day: 0, tod: 3600 }), 200.0);
    }

    #[test]
    fn line_names_are_compared_without_punctuation_or_leading_zeros() {
        assert_eq!(norm("Airport Exp.-4"), norm("AIRPORT-EXP-04"));
        assert_eq!(norm("0901 NS"), "901NS");
        assert_eq!(norm("990 Ext."), norm("990EXTN"));
        assert_eq!(norm("405"), "405");
        assert_eq!(norm("100"), "100");
        assert_eq!(base("425CLUP"), "425CL");
        assert_eq!(base("405DOWN"), "405");
    }
}
