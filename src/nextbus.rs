//! Replay test of the live next-bus predictions.
//!
//! At past moments T we rebuild exactly what the server would have known —
//! the feed snapshots fetched up to T and a model fitted on data before the
//! cutoff — and score two things against what buses then did:
//!
//!   A. per bus:   predicted arrival of each tracked bus at each stop ahead
//!                 of it, against when that bus really reached the stop;
//!   B. per rider: "when is the next bus on this route at this stop?",
//!                 live timetable against the static one, against the first
//!                 bus that really came.
//!
//!   C. per standing bus: a bus standing at its route's first stop is given
//!                 an estimated departure; scored where that bus was next timed.
//!
//! Truth comes from the day's traversals, which are extracted with hindsight.
//! A stop crossing is only "trusted" if the GPS points around it were close
//! in time, so some real arrivals are missing from the truth; the report says
//! how often no arrival was found.

use crate::clock::Local;
use crate::eval::{Acc, Rows, line};
use crate::gtfs::Network;
use crate::link;
use crate::live::{Blended, LiveRatios, Service, WINDOW_S};
use crate::model::Model;
use crate::raptor::{Scheduled, SegTimes, Timetable};
use crate::rt;
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

/// Replay moments are this far apart.
const STEP_S: i64 = 600;
/// Predictions further ahead than this are not scored.
const MAX_LEAD_S: f64 = 3600.0;
/// A bus that has not reached a stop this long after the predicted time is
/// counted as "no arrival found" rather than scored.
const TRUTH_SLACK_S: f64 = 3600.0;
/// Rider test: the real next bus must come within this long.
const RIDER_WINDOW_S: f64 = 3600.0;
/// A silence longer than this between snapshots is a collector gap.
const GAP_S: i64 = 90;

/// Lead-time buckets, minutes.
const BUCKETS: [(f64, f64); 5] = [(0.0, 5.0), (5.0, 10.0), (10.0, 20.0), (20.0, 40.0), (40.0, 60.0)];

/// When stops were really reached: (route, stop position) -> (time, vehicle), sorted by time.
struct Truth {
    at: HashMap<(u32, u16), Vec<(f64, u32)>>,
    vehicles: HashMap<String, u32>,
}

impl Truth {
    fn build(data: &Rows) -> Truth {
        let vehicles: HashMap<String, u32> = data.vehicles.iter().enumerate().map(|(i, v)| (v.clone(), i as u32)).collect();
        let mut at: HashMap<(u32, u16), Vec<(f64, u32)>> = HashMap::new();
        for r in &data.rows {
            let (route, v) = (r.route, r.vehicle);
            // The hop's start is the arrival at stop k (except k = 0, where it
            // is the departure); its end is the arrival at stop k + 1.
            if r.k > 0 {
                at.entry((route, r.k)).or_default().push((r.t_enter, v));
            }
            at.entry((route, r.k + 1)).or_default().push((r.t_enter + r.obs.dur as f64, v));
        }
        for v in at.values_mut() {
            v.sort_by(|a, b| a.0.total_cmp(&b.0));
            v.dedup_by(|a, b| a.1 == b.1 && (a.0 - b.0).abs() < 1.0);
        }
        Truth { at, vehicles }
    }

    /// First time after `after` that `vehicle` reached the stop, if before `until`.
    fn vehicle_at(&self, route: u32, k: u16, vehicle: &str, after: f64, until: f64) -> Option<f64> {
        let v = *self.vehicles.get(vehicle)?;
        let list = self.at.get(&(route, k))?;
        let i = list.partition_point(|x| x.0 <= after);
        list[i..].iter().take_while(|x| x.0 <= until).find(|x| x.1 == v).map(|x| x.0)
    }

    /// First time after `after` that any bus reached the stop, if before `until`.
    fn any_at(&self, route: u32, k: u16, after: f64, until: f64) -> Option<f64> {
        let list = self.at.get(&(route, k))?;
        list.get(list.partition_point(|x| x.0 <= after)).map(|x| x.0).filter(|&t| t <= until)
    }
}

/// What kind of trip a timetable row is.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    /// A real bus on its way along the route.
    Tracked = 0,
    /// A real bus standing at the first stop, with an estimated departure.
    Standing = 1,
    /// A timetabled start.
    Timetabled = 2,
}

/// Earliest arrival at stop position `k` of a route at or after `tod`, and
/// what kind of trip it is.
fn next_arrival(tt: &Timetable, route: usize, n: usize, k: usize, tod: u32) -> Option<(u32, Kind)> {
    let arr = &tt.arr[route];
    let mut best: Option<(u32, Kind)> = None;
    for trip in 0..arr.len() / n {
        let a = arr[trip * n + k];
        if a != 0 && a >= tod && best.is_none_or(|b| a < b.0) {
            let kind = match tt.tracked.get(route).and_then(|t| t.get(trip)).copied().flatten() {
                None => Kind::Timetabled,
                Some(_) if tt.is_standing(route as u32, trip as u32) => Kind::Standing,
                Some(_) => Kind::Tracked,
            };
            best = Some((a, kind));
        }
    }
    best
}

/// The same over every trip of the GTFS route: its short-turn trips call at
/// the route's first stops too, and a rider there can board any of them.
fn next_on_route(net: &Network, tt: &Timetable, route: usize, k: usize, tod: u32) -> Option<(u32, Kind)> {
    let stop = net.routes[route].stops[k];
    net.family(route as u32)
        .filter(|&e| net.routes[e as usize].stops.get(k) == Some(&stop))
        .filter_map(|e| next_arrival(tt, e as usize, net.routes[e as usize].stops.len(), k, tod))
        .min_by_key(|x| x.0)
}

fn bucket(lead_s: f64) -> Option<usize> {
    BUCKETS.iter().position(|&(lo, hi)| lead_s >= lo * 60.0 && lead_s < hi * 60.0)
}

pub fn replay(net: &Network, data: &Rows, raw_dir: &Path, cutoff: i64, until: i64) -> Result<String> {
    let rows = &data.rows;
    let mut text = String::new();
    let train: Vec<_> = rows.iter().filter(|r| r.obs.t_enter < cutoff).map(|r| r.obs).collect();
    let model: Arc<Model> = Arc::new(Model::fit(net, &train));
    let truth = Truth::build(data);
    let data_end = rows.iter().map(|r| r.t_enter + r.obs.dur as f64).fold(0.0, f64::max).min(until as f64);
    text += &format!("model fitted on {} traversals before {}; truth runs to {}\n", train.len(), Local::from_epoch(cutoff).iso(), Local::from_epoch(data_end as i64).iso());

    // Every archived snapshot, and the silences between them.
    let mut files = Vec::new();
    for d in rt::list_dates(raw_dir)? {
        files.extend(rt::list_day(raw_dir, &d)?);
    }
    files.sort();
    let gaps: Vec<(f64, f64)> = files.windows(2).filter(|w| w[1].0 - w[0].0 > GAP_S).map(|w| (w[0].0 as f64, w[1].0 as f64)).collect();
    // A stop crossing next to a silence is not trusted, so stay clear of them.
    let near_gap = |a: f64, b: f64| gaps.iter().any(|&(g0, g1)| a < g1 + 120.0 && b > g0 - 120.0);

    let sched = Scheduled;
    let names = ["bus position + timetable speed", "bus position + model", "bus position + model + live"];
    let mut per_bus: Vec<Vec<Acc>> = (0..3).map(|_| BUCKETS.iter().map(|_| Acc::default()).collect()).collect();
    let mut all: Vec<Acc> = (0..3).map(|_| Acc::default()).collect();
    let (mut scored, mut not_found, mut passed_untimed) = (0usize, 0usize, 0usize);
    let mut buses_scored: HashSet<(String, i64)> = HashSet::new();
    // Rider test: [tracked bus approaching, standing bus is next, timetabled start is next]
    //           x [live timetable, the same without standing buses, static].
    let mut rider: [[Acc; 3]; 3] = Default::default();
    let (mut rider_none, mut moments) = (0usize, 0usize);
    // Where no tracked bus is approaching: by how the last hour's real service compares with the timetable.
    let mut by_service: [(Acc, usize); 4] = Default::default();
    // Standing buses: predicted against actual time at the first stop where the bus was next timed.
    let (mut standing_acc, mut standing_gone, mut standing_n) = (Acc::default(), 0usize, 0usize);

    let mut t = cutoff.div_euclid(STEP_S) * STEP_S + STEP_S;
    while (t as f64) < data_end - 900.0 {
        let now = t;
        t += STEP_S;
        let lo = files.partition_point(|f| f.0 < now - WINDOW_S - 900);
        let hi = files.partition_point(|f| f.0 <= now);
        if hi == 0 || now - files[hi - 1].0 > 60 || lo >= hi {
            continue; // the collector was not running at this moment
        }
        let (trav, buses) = link::scan(net, &files[lo..hi], now)?;
        if buses.is_empty() {
            continue;
        }
        moments += 1;
        let day = Local::from_epoch(now);
        let (dt, now_tod) = (day.day_type(), day.tod);
        let midnight = (now - now_tod as i64) as f64;
        let live = Arc::new(LiveRatios::compute(net, model.as_ref(), &trav, now));
        let blended = Blended { base: model.clone(), live };
        let providers: [&dyn SegTimes; 3] = [&sched, model.as_ref(), &blended];
        let tts: Vec<Timetable> = providers.iter().map(|p| Timetable::build_live(net, *p, dt, now, &buses)).collect();
        let static_tt = Timetable::build(net, model.as_ref(), dt);
        // The live timetable as it was before standing buses were used.
        let moving: Vec<link::VehicleState> = buses.iter().filter(|b| b.waiting_s.is_none()).cloned().collect();
        let old_tt = Timetable::build_live(net, &blended, dt, now, &moving);
        let service = Service::build(net, &trav, &buses, now);

        for (ri, r) in net.routes.iter().enumerate() {
            let n = r.stops.len();
            let tracked = &tts[2].tracked[ri];
            if !tracked.iter().any(|x| x.is_some()) {
                continue;
            }
            // ---- C. a bus standing at the first stop: where it was next timed
            for (trip, who) in tracked.iter().enumerate() {
                let Some(vi) = who else { continue };
                if !tts[2].standing[*vi as usize] || (now as f64) + RIDER_WINDOW_S > data_end || near_gap(now as f64, now as f64 + RIDER_WINDOW_S) {
                    continue;
                }
                standing_n += 1;
                let (name, ts) = (&tts[2].vehicles[*vi as usize], buses[*vi as usize].ts as f64);
                match (1..n).find_map(|k| truth.vehicle_at(ri as u32, k as u16, name, ts, now as f64 + RIDER_WINDOW_S).map(|t| (k, t))) {
                    Some((k, actual)) => standing_acc.push((midnight + tts[2].arr[ri][trip * n + k] as f64 - now as f64) as f32, (actual - now as f64) as f32),
                    None => standing_gone += 1,
                }
            }
            // ---- A. each tracked bus at each stop ahead of it
            for (trip, who) in tracked.iter().enumerate() {
                let Some(vi) = who else { continue };
                if tts[2].standing[*vi as usize] {
                    continue;
                }
                let name = &tts[2].vehicles[*vi as usize];
                for k in 1..n {
                    let a = tts[2].arr[ri][trip * n + k];
                    if a == 0 {
                        continue;
                    }
                    let pred = midnight + a as f64;
                    let lead = pred - now as f64;
                    let Some(b) = bucket(lead) else { continue };
                    if lead > MAX_LEAD_S || pred + TRUTH_SLACK_S > data_end || near_gap(now as f64, pred + TRUTH_SLACK_S) {
                        continue;
                    }
                    let (ts, until) = (buses[*vi as usize].ts as f64, pred + TRUTH_SLACK_S);
                    let Some(actual) = truth.vehicle_at(ri as u32, k as u16, name, ts, until) else {
                        not_found += 1;
                        // Was the bus timed at a later stop (so only this stop's time is missing)?
                        if (k + 1..n).any(|j| truth.vehicle_at(ri as u32, j as u16, name, ts, until).is_some()) {
                            passed_untimed += 1;
                        }
                        continue;
                    };
                    scored += 1;
                    buses_scored.insert((name.clone(), now));
                    for (p, tt) in tts.iter().enumerate() {
                        // Minutes from now until arrival: predicted against actual.
                        let pr = (midnight + tt.arr[ri][trip * n + k] as f64 - now as f64) as f32;
                        let ac = (actual - now as f64) as f32;
                        per_bus[p][b].push(pr, ac);
                        all[p].push(pr, ac);
                    }
                }
            }
            // ---- B. the rider's question: when is the next bus here?
            if (now as f64) + RIDER_WINDOW_S > data_end || near_gap(now as f64, now as f64 + RIDER_WINDOW_S) {
                continue;
            }
            for k in 1..n {
                let (Some((lv, kind)), Some((old, _)), Some((st, _))) =
                    (next_on_route(net, &tts[2], ri, k, now_tod), next_on_route(net, &old_tt, ri, k, now_tod), next_on_route(net, &static_tt, ri, k, now_tod))
                else {
                    continue;
                };
                // Real buses past this stop in the last hour against the timetable's count.
                let class = (kind != Kind::Tracked && k < n - 1).then(|| {
                    let (seen, due) = (service.seen(ri as u32, k), Service::timetabled(net, ri as u32, k, now_tod));
                    match (seen, due) {
                        (_, 0..=1) => 3,
                        (0, _) => 2,
                        (s, d) if s * 2 < d => 1,
                        _ => 0,
                    }
                });
                let Some(actual) = truth.any_at(ri as u32, k as u16, now as f64, now as f64 + RIDER_WINDOW_S) else {
                    rider_none += 1;
                    if let Some(c) = class {
                        by_service[c].1 += 1;
                    }
                    continue;
                };
                let wait = (actual - now as f64) as f32;
                let g = kind as usize;
                rider[g][0].push((lv - now_tod) as f32, wait);
                rider[g][1].push((old - now_tod) as f32, wait);
                rider[g][2].push((st - now_tod) as f32, wait);
                if let Some(c) = class {
                    by_service[c].0.push((lv - now_tod) as f32, wait);
                }
            }
        }
    }

    text += &format!("{moments} replay moments, {STEP_S} s apart\n");
    text += &format!(
        "\nA. Tracked buses: predicted against actual arrival at the stops ahead, minutes\n   {} bus-and-stop predictions from {} bus sightings; no trusted arrival found for a further {} ({:.0}%)\n   of those, {:.0}% were timed at a later stop (this stop's time fell in a GPS gap); for the rest the bus was not timed again on this route\n",
        scored,
        buses_scored.len(),
        not_found,
        100.0 * not_found as f64 / (scored + not_found).max(1) as f64,
        100.0 * passed_untimed as f64 / not_found.max(1) as f64
    );
    for (p, name) in names.iter().enumerate() {
        text += &line(name, &all[p], 60.0, "m");
        text.push('\n');
    }
    text += "\n   by how far ahead the arrival was predicted (model + live):\n";
    for (b, &(lo, hi)) in BUCKETS.iter().enumerate() {
        text += &line(&format!("{lo:.0}-{hi:.0} min ahead"), &per_bus[2][b], 60.0, "m");
        text.push('\n');
    }
    text += "\n   same buckets, bus position + timetable speed:\n";
    for (b, &(lo, hi)) in BUCKETS.iter().enumerate() {
        text += &line(&format!("{lo:.0}-{hi:.0} min ahead"), &per_bus[0][b], 60.0, "m");
        text.push('\n');
    }
    text += &format!(
        "\nB. A rider at a stop asks when the next bus on the route comes: predicted wait against the real wait, minutes\n   routes with a tracked bus only; stops where no bus was seen within {:.0} min are left out ({rider_none} cases)\n",
        RIDER_WINDOW_S / 60.0
    );
    let titles = [
        "a tracked bus is approaching the stop",
        "the next bus is one standing at the route's first stop",
        "the next bus is a timetabled start",
    ];
    for (g, title) in titles.iter().enumerate() {
        text += &format!("   {title} (mean real wait {:.1} min):\n", rider[g][0].mean_actual() / 60.0);
        text += &line("live timetable", &rider[g][0], 60.0, "m");
        text.push('\n');
        text += &line("live, standing buses not used", &rider[g][1], 60.0, "m");
        text.push('\n');
        text += &line("static timetable (model speeds)", &rider[g][2], 60.0, "m");
        text.push('\n');
    }
    text += "\n   no tracked bus approaching, by buses really seen at the stop in the last hour against the timetable's count\n   (predicted = live timetable; 'none came' = no bus within the hour, not in the averages):\n";
    let classes = ["at least half the timetabled number", "fewer than half", "none seen", "timetable has under 2 an hour"];
    for (c, title) in classes.iter().enumerate() {
        let (acc, none) = &by_service[c];
        let n = acc.stats().map_or(0, |s| s.0);
        text += &line(title, acc, 60.0, "m");
        text += &format!("  mean real wait {:.1}m  none came {:.0}%\n", acc.mean_actual() / 60.0, 100.0 * *none as f64 / (n + none).max(1) as f64);
    }
    text += &format!(
        "\nC. Buses standing at their route's first stop, given an estimated departure: minutes from now until the first stop where the bus was next timed\n   {standing_n} standing buses; {standing_gone} ({:.0}%) were not timed on the route within {:.0} min\n",
        100.0 * standing_gone as f64 / standing_n.max(1) as f64,
        RIDER_WINDOW_S / 60.0
    );
    text += &line("standing bus", &standing_acc, 60.0, "m");
    text.push('\n');
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truth_finds_the_first_arrival_after_a_moment() {
        let mut at = HashMap::new();
        at.insert((7u32, 3u16), vec![(100.0, 0u32), (400.0, 1), (900.0, 0)]);
        let vehicles = HashMap::from([("a".to_string(), 0u32), ("b".to_string(), 1u32)]);
        let t = Truth { at, vehicles };
        // Any bus: the next one after t = 150 is bus b at 400.
        assert_eq!(t.any_at(7, 3, 150.0, 1000.0), Some(400.0));
        // Bus a specifically: its next pass is at 900.
        assert_eq!(t.vehicle_at(7, 3, "a", 150.0, 1000.0), Some(900.0));
        // ...but not if we stop looking before then.
        assert_eq!(t.vehicle_at(7, 3, "a", 150.0, 800.0), None);
        assert_eq!(t.any_at(7, 3, 950.0, 2000.0), None);
    }

    #[test]
    fn next_arrival_skips_passed_stops_and_past_times() {
        // One route, 2 stops, 3 rows: a tracked bus already past stop 1 (0),
        // a tracked bus due at 500, a scheduled one at 450.
        let mut tt = Timetable {
            arr: vec![vec![0, 0, 100, 500, 300, 450]],
            tracked: vec![vec![Some(0), Some(1), None]],
            vehicles: vec!["a".into(), "b".into()],
            standing: vec![false, false],
        };
        assert_eq!(next_arrival(&tt, 0, 2, 1, 400), Some((450, Kind::Timetabled)));
        assert_eq!(next_arrival(&tt, 0, 2, 1, 460), Some((500, Kind::Tracked)));
        assert_eq!(next_arrival(&tt, 0, 2, 1, 600), None);
        tt.standing[1] = true;
        assert_eq!(next_arrival(&tt, 0, 2, 1, 460), Some((500, Kind::Standing)));
    }
}
