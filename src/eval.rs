//! Backtest: fit on earlier traversals, predict later ones, compare with
//! what the buses actually did.
//!
//! The split is by time, never random: a random split would put parts of
//! the same bus run on both sides and flatter the model.
//!
//! Two views are reported. Segment level is one stop-to-stop hop; it is noisy
//! by nature (a 22 s GPS interval against hops of about a minute). Trip level
//! is a chain of consecutive hops ridden by one bus, predicted the way the
//! planner does it — each hop at the time the bus is predicted to get there —
//! and is the number that matters to a rider.

use crate::clock::Local;
use crate::gtfs::Network;
use crate::link::Traversal;
use crate::live::{Blended, LiveParams, LiveRatios, WINDOW_S};
use crate::model::{Model, Obs, Params};
use crate::raptor::{Scheduled, SegTimes};
use std::sync::Arc;
use anyhow::{Context, Result};
use rayon::prelude::*;
use std::collections::HashMap;
use std::path::Path;

/// One traversal with enough identity to rebuild the run it came from.
/// Kept small: a month of archive is tens of millions of these.
#[derive(Clone, Copy, Debug)]
pub struct Row {
    pub obs: Obs,
    pub t_enter: f64,
    pub k: u16,
    /// Index into the network's routes.
    pub route: u32,
    /// Index into `Rows::vehicles`.
    pub vehicle: u32,
    /// One bus on one run (vehicle, dispatch id, route), numbered across the whole load.
    pub run: u32,
}

/// Every traversal on disk, with the vehicle names the rows refer to.
#[derive(Default)]
pub struct Rows {
    pub rows: Vec<Row>,
    pub vehicles: Vec<String>,
}

/// Read traversal CSVs written by `extract` and `import`, one thread per file.
pub fn load_rows(net: &Network, dir: &Path) -> Result<Rows> {
    let mut out = Rows::default();
    if !dir.is_dir() {
        return Ok(out);
    }
    let mut files: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| !n.starts_with('.') && n.ends_with(".csv")))
        .collect();
    files.sort();
    // Per file: rows with file-local vehicle and run numbers, and the vehicle names.
    let parts: Vec<(Vec<Row>, Vec<String>, u32)> = files
        .par_iter()
        .map(|p| -> Result<_> {
            let mut rows = Vec::new();
            let mut vehicles: Vec<String> = Vec::new();
            let mut vidx: HashMap<String, u32> = HashMap::new();
            let mut runs: HashMap<(u32, String, u32), u32> = HashMap::new();
            let mut rd = csv::Reader::from_path(p).with_context(|| format!("opening {}", p.display()))?;
            let mut r = csv::StringRecord::new();
            while rd.read_record(&mut r)? {
                let (Some(&a), Some(&b)) = (net.stop_index.get(&r[2]), net.stop_index.get(&r[3])) else { continue };
                let (Some(&seg), Some(&route)) = (net.seg_index.get(&(a, b)), net.route_index.get(&r[4])) else { continue };
                let t: f64 = r[0].parse()?;
                let vehicle = match vidx.get(&r[8]) {
                    Some(&v) => v,
                    None => {
                        vehicles.push(r[8].to_string());
                        vidx.insert(r[8].to_string(), vehicles.len() as u32 - 1);
                        vehicles.len() as u32 - 1
                    }
                };
                let n = runs.len() as u32;
                let run = *runs.entry((vehicle, r[9].to_string(), route)).or_insert(n);
                rows.push(Row { obs: Obs { seg, t_enter: t as i64, dur: r[1].parse()? }, t_enter: t, k: r[5].parse()?, route, vehicle, run });
            }
            Ok((rows, vehicles, runs.len() as u32))
        })
        .collect::<Result<_>>()?;
    let mut vidx: HashMap<String, u32> = HashMap::new();
    let mut run0 = 0u32;
    out.rows.reserve(parts.iter().map(|p| p.0.len()).sum());
    for (rows, vehicles, n_runs) in parts {
        let global: Vec<u32> = vehicles
            .into_iter()
            .map(|v| match vidx.get(&v) {
                Some(&i) => i,
                None => {
                    out.vehicles.push(v.clone());
                    vidx.insert(v, out.vehicles.len() as u32 - 1);
                    out.vehicles.len() as u32 - 1
                }
            })
            .collect();
        out.rows.extend(rows.into_iter().map(|r| Row { vehicle: global[r.vehicle as usize], run: run0 + r.run, ..r }));
        run0 += n_runs;
    }
    Ok(out)
}

/// Error statistics for a set of (predicted, actual) pairs, in seconds.
#[derive(Default)]
pub struct Acc {
    pairs: Vec<(f32, f32)>,
}

impl Acc {
    pub fn push(&mut self, pred: f32, actual: f32) {
        self.pairs.push((pred, actual));
    }

    /// Mean of the actual values, seconds.
    pub fn mean_actual(&self) -> f32 {
        self.pairs.iter().map(|p| p.1).sum::<f32>() / self.pairs.len().max(1) as f32
    }

    /// (n, MAE s, MAPE %, median abs error s, p90 abs error s, bias s).
    /// Bias is mean(predicted - actual): negative means we under-predict.
    pub fn stats(&self) -> Option<(usize, f32, f32, f32, f32, f32)> {
        let n = self.pairs.len();
        if n == 0 {
            return None;
        }
        let mut abs: Vec<f32> = self.pairs.iter().map(|(p, a)| (p - a).abs()).collect();
        abs.sort_by(|a, b| a.total_cmp(b));
        let mae = abs.iter().sum::<f32>() / n as f32;
        let mape = 100.0 * self.pairs.iter().map(|(p, a)| (p - a).abs() / a.max(1.0)).sum::<f32>() / n as f32;
        let bias = self.pairs.iter().map(|(p, a)| p - a).sum::<f32>() / n as f32;
        Some((n, mae, mape, abs[n / 2], abs[(n * 9 / 10).min(n - 1)], bias))
    }
}

pub fn line(label: &str, a: &Acc, unit: f32, u: &str) -> String {
    match a.stats() {
        None => format!("  {label:<34} (no data)"),
        Some((n, mae, mape, med, p90, bias)) => format!(
            "  {label:<34} n={n:<8} MAE {:>6.1}{u}  MAPE {mape:>5.1}%  median {:>6.1}{u}  p90 {:>6.1}{u}  bias {:>+6.1}{u}",
            mae / unit,
            med / unit,
            p90 / unit,
            bias / unit
        ),
    }
}

/// A chain of consecutive segments ridden by one bus.
struct Chain<'a> {
    rows: Vec<&'a Row>,
}

/// Group test rows into runs and cut each into unbroken chains: consecutive
/// stop positions where each hop starts when the previous one ended.
fn chains(rows: &[Row]) -> Vec<Chain<'_>> {
    let mut by_run: HashMap<u32, Vec<&Row>> = HashMap::new();
    for r in rows {
        by_run.entry(r.run).or_default().push(r);
    }
    let mut out = Vec::new();
    for (_, mut v) in by_run {
        v.sort_by(|a, b| a.t_enter.total_cmp(&b.t_enter));
        let mut cur: Vec<&Row> = Vec::new();
        for r in v {
            let joins = cur.last().is_some_and(|p| r.k == p.k + 1 && (r.t_enter - (p.t_enter + p.obs.dur as f64)).abs() < 1.5);
            if !joins && !cur.is_empty() {
                out.push(Chain { rows: std::mem::take(&mut cur) });
            }
            cur.push(r);
        }
        if !cur.is_empty() {
            out.push(Chain { rows: cur });
        }
    }
    out
}

/// Predict a chain the way the planner would: each hop at the time the bus
/// is predicted (not observed) to reach it.
fn predict_chain(net: &Network, times: &dyn SegTimes, c: &[&Row]) -> f32 {
    let start = Local::from_epoch(c[0].obs.t_enter);
    let dt = start.day_type();
    let mut t = start.tod as f32;
    for r in c {
        t += times.secs(net, r.obs.seg, dt, (t as u32) % 86_400);
    }
    t - start.tod as f32
}

pub struct Report {
    pub text: String,
}

/// Fit on rows before `cutoff` (epoch seconds), test on those from then until `until`.
pub fn backtest(net: &Network, rows: &[Row], cutoff: i64, until: i64) -> Report {
    let train: Vec<&Row> = rows.iter().filter(|r| r.obs.t_enter < cutoff).collect();
    let test: Vec<&Row> = rows.iter().filter(|r| r.obs.t_enter >= cutoff && r.obs.t_enter < until).collect();
    let model = Arc::new(Model::fit(net, &train.iter().map(|r| r.obs).collect::<Vec<_>>()));
    let sched = Scheduled;
    let providers: [(&str, &dyn SegTimes); 2] = [("timetable (18 km/h)", &sched), ("model", model.as_ref())];
    let mut text = String::new();
    let span = |v: &[&Row]| {
        if v.is_empty() {
            return "none".to_string();
        }
        let (a, b) = (v.iter().map(|r| r.obs.t_enter).min().unwrap(), v.iter().map(|r| r.obs.t_enter).max().unwrap());
        format!("{} to {}", Local::from_epoch(a).iso(), Local::from_epoch(b).iso())
    };
    text += &format!("train: {} traversals, {}\n", train.len(), span(&train));
    text += &format!("test:  {} traversals, {}\n", test.len(), span(&test));
    text += &format!("model fitted on {} usable traversals covering {} of {} segments\n", model.n_obs, model.segments_with_data(), net.segments.len());
    if test.is_empty() || train.is_empty() {
        text += "\nNothing to evaluate: one side of the split is empty.\n";
        return Report { text };
    }

    // ---- segment level
    text += "\nSegment level (one stop-to-stop hop), seconds\n";
    for (name, p) in providers {
        let mut a = Acc::default();
        for r in &test {
            let t = Local::from_epoch(r.obs.t_enter);
            a.push(p.secs(net, r.obs.seg, t.day_type(), t.tod), r.obs.dur);
        }
        text += &line(name, &a, 1.0, "s");
        text.push('\n');
    }
    // How much of the test set the model had direct evidence for.
    let mut lv: HashMap<u8, usize> = HashMap::new();
    for r in &test {
        let t = Local::from_epoch(r.obs.t_enter);
        *lv.entry(model.level(r.obs.seg, t.day_type(), t.tod)).or_default() += 1;
    }
    let mut lv: Vec<_> = lv.into_iter().collect();
    lv.sort();
    text += "  test traversals by model evidence level: ";
    text += &lv.iter().map(|(l, n)| format!("L{l} {:.0}%", 100.0 * *n as f32 / test.len() as f32)).collect::<Vec<_>>().join(", ");
    text.push('\n');

    // ---- trip level
    let test_owned: Vec<Row> = test.iter().map(|r| **r).collect();
    let all = chains(&test_owned);
    for (title, min_hops, max_hops) in [("Trip level: 5-14 consecutive hops", 5, 14), ("Trip level: 15+ consecutive hops", 15, usize::MAX)] {
        let sel: Vec<&Chain> = all.iter().filter(|c| (min_hops..=max_hops).contains(&c.rows.len())).collect();
        let mean_min = sel.iter().map(|c| c.rows.iter().map(|r| r.obs.dur).sum::<f32>()).sum::<f32>() / sel.len().max(1) as f32 / 60.0;
        if sel.is_empty() {
            text += &format!("\n{title}: no chains that long in the test period\n");
            continue;
        }
        text += &format!("\n{title} ({} chains, mean actual {:.1} min), minutes\n", sel.len(), mean_min);
        for (name, p) in providers {
            let mut a = Acc::default();
            for c in &sel {
                a.push(predict_chain(net, p, &c.rows), c.rows.iter().map(|r| r.obs.dur).sum());
            }
            text += &line(name, &a, 60.0, "m");
            text.push('\n');
        }
    }

    // ---- by time of day, for chains of 5+ hops
    text += "\nTrip level by start hour (chains of 5+ hops), MAE minutes: timetable vs model\n";
    let mut by_hour: Vec<(Acc, Acc)> = (0..24).map(|_| (Acc::default(), Acc::default())).collect();
    for c in all.iter().filter(|c| c.rows.len() >= 5) {
        let h = (Local::from_epoch(c.rows[0].obs.t_enter).tod / 3600) as usize;
        let actual: f32 = c.rows.iter().map(|r| r.obs.dur).sum();
        by_hour[h].0.push(predict_chain(net, &sched, &c.rows), actual);
        by_hour[h].1.push(predict_chain(net, model.as_ref(), &c.rows), actual);
    }
    for (h, (s, m)) in by_hour.iter().enumerate() {
        if let (Some(s), Some(m)) = (s.stats(), m.stats()) {
            text += &format!("  {h:02}:00  n={:<6} timetable {:>5.1}m (bias {:>+5.1}m)   model {:>5.1}m (bias {:>+5.1}m)\n", s.0, s.1 / 60.0, s.5 / 60.0, m.1 / 60.0, m.5 / 60.0);
        }
    }
    text += &live_replay(net, rows, &all, model, until);
    Report { text }
}

/// The feed runs about this far behind real time (measured),
/// so a prediction made at time T only knows traversals finished by T - this.
const FEED_LATENCY_S: i64 = 100;
/// Live ratios are recomputed on this grid during the replay.
const REPLAY_STEP_S: i64 = 300;

/// Replay the archive in time order: for each test chain, build the live view
/// from only what had been observed before the bus set off, and compare
/// history-only with history + live. Each base is tested with and without.
fn live_replay(net: &Network, rows: &[Row], chains: &[Chain], model: Arc<Model>, until: i64) -> String {
    let mut text = String::from("\nLive blending, replayed (chains of 5+ hops), minutes\n");
    let mut sel: Vec<&Chain> = chains.iter().filter(|c| c.rows.len() >= 5).collect();
    if sel.is_empty() {
        return text + "  no chains to replay\n";
    }
    sel.sort_by_key(|c| c.rows[0].obs.t_enter);
    // Traversals (train and test), ordered by when they finished.
    // Only those recent enough to be in some test chain's live window.
    let from = sel[0].rows[0].obs.t_enter - WINDOW_S - 2 * REPLAY_STEP_S;
    let mut trav: Vec<Traversal> = rows
        .iter()
        .filter(|r| r.obs.t_enter >= from && r.obs.t_enter < until)
        .map(|r| Traversal { seg: r.obs.seg, route: 0, k: r.k, t_enter: r.t_enter, dur: r.obs.dur, gap: 0.0 })
        .collect();
    trav.sort_by(|a, b| (a.t_enter + a.dur as f64).total_cmp(&(b.t_enter + b.dur as f64)));
    let done: Vec<f64> = trav.iter().map(|t| t.t_enter + t.dur as f64).collect();

    let bases: [(&str, Arc<dyn SegTimes>); 2] = [("timetable", Arc::new(Scheduled)), ("model", model)];
    let mut acc: Vec<(Acc, Acc)> = bases.iter().map(|_| (Acc::default(), Acc::default())).collect();
    // Per day: timetable, model, model + live.
    let mut by_day: std::collections::BTreeMap<i64, [Acc; 3]> = Default::default();
    let mut i = 0;
    while i < sel.len() {
        // All chains starting in the same grid step share one live view.
        let slot = sel[i].rows[0].obs.t_enter.div_euclid(REPLAY_STEP_S) * REPLAY_STEP_S;
        let known_until = slot - FEED_LATENCY_S;
        let lo = done.partition_point(|&d| d < (known_until - WINDOW_S) as f64);
        let hi = done.partition_point(|&d| d <= known_until as f64);
        let mut j = i;
        while j < sel.len() && sel[j].rows[0].obs.t_enter < slot + REPLAY_STEP_S {
            j += 1;
        }
        for (b, (_, base)) in bases.iter().enumerate() {
            let live = Arc::new(LiveRatios::compute(net, base.as_ref(), &trav[lo..hi], known_until));
            let blended = Blended { base: base.clone(), live };
            for c in &sel[i..j] {
                let actual: f32 = c.rows.iter().map(|r| r.obs.dur).sum();
                let (plain, with_live) = (predict_chain(net, base.as_ref(), &c.rows), predict_chain(net, &blended, &c.rows));
                acc[b].0.push(plain, actual);
                acc[b].1.push(with_live, actual);
                let d = by_day.entry(Local::from_epoch(c.rows[0].obs.t_enter).day).or_default();
                if b == 0 {
                    d[0].push(plain, actual);
                } else {
                    d[1].push(plain, actual);
                    d[2].push(with_live, actual);
                }
            }
        }
        i = j;
    }
    for ((name, _), (plain, with_live)) in bases.iter().zip(&acc) {
        text += &line(&format!("{name}, history only"), plain, 60.0, "m");
        text.push('\n');
        text += &line(&format!("{name} + live"), with_live, 60.0, "m");
        text.push('\n');
    }
    text += "\nBy day (chains of 5+ hops): MAE minutes (bias)\n";
    text += "  date        kind        chains   timetable        model            model + live\n";
    for (day, a) in &by_day {
        let l = Local { day: *day, tod: 0 };
        let cell = |x: &Acc| x.stats().map_or(String::new(), |s| format!("{:5.1} ({:>+5.1})", s.1 / 60.0, s.5 / 60.0));
        text += &format!(
            "  {}  {:<9} {:>8}   {}    {}    {}\n",
            l.date_string(),
            crate::clock::DAY_KIND_NAMES[l.day_type()],
            a[1].pairs.len(),
            cell(&a[0]),
            cell(&a[1]),
            cell(&a[2])
        );
    }
    text
}

/// Compare model settings by cross-validation. Each fold is a span of days
/// held out in turn; the model is fitted on every other row before
/// `data_until` and scored on the fold's chains of 5+ hops. Rows from
/// `data_until` on are never touched, so the final backtest stays clean.
/// (Fitting on days after the held-out ones is fine for choosing settings of
/// a time-of-day profile; the final backtest is strictly past-to-future.)
pub fn tune(net: &Network, rows: &[Row], folds: &[(i64, i64)], data_until: i64, variants: &[(String, Params)]) -> String {
    let in_fold = |t: i64, f: &(i64, i64)| t >= f.0 && t < f.1;
    // Per fold: training observations and validation chains.
    let train: Vec<Vec<Obs>> = folds.iter().map(|f| rows.iter().filter(|r| r.obs.t_enter < data_until && !in_fold(r.obs.t_enter, f)).map(|r| r.obs).collect()).collect();
    let valid: Vec<Vec<Row>> = folds.iter().map(|f| rows.iter().filter(|r| in_fold(r.obs.t_enter, f)).copied().collect()).collect();
    let all: Vec<Vec<Chain>> = valid.iter().map(|v| chains(v)).collect();
    let sel: Vec<Vec<&Chain>> = all.iter().map(|a| a.iter().filter(|c| c.rows.len() >= 5).collect()).collect();
    let mut text = String::new();
    for (i, f) in folds.iter().enumerate() {
        text += &format!("fold {}: held out {} to {}, {} chains of 5+ hops, fitted on {} traversals\n", i + 1, Local::from_epoch(f.0).date_string(), Local::from_epoch(f.1 - 1).date_string(), sel[i].len(), train[i].len());
    }
    text += "MAE in minutes over all folds; bias = predicted - actual; evening = chains starting 17:00-20:00\n";
    text += &format!("{:<34} {:>5} {:>6} {:>6}  | {:>5} {:>5} {:>5} {:>5} | {:>5} {:>6} | MAE per fold\n", "variant", "MAE", "MAPE%", "bias", "wkday", "sat", "sun", "hol", "eve", "bias");
    for (name, p) in variants {
        let mut a = Acc::default();
        let mut kinds: [Acc; 4] = Default::default();
        let mut eve = Acc::default();
        let mut per_fold = Vec::new();
        for i in 0..folds.len() {
            let model = Model::fit_with(net, &train[i], p);
            // (predicted, actual, day kind, start hour)
            let res: Vec<(f32, f32, usize, u32)> = sel[i]
                .par_iter()
                .map(|c| {
                    let t = Local::from_epoch(c.rows[0].obs.t_enter);
                    (predict_chain(net, &model, &c.rows), c.rows.iter().map(|r| r.obs.dur).sum(), t.day_type(), t.tod / 3600)
                })
                .collect();
            let mut fa = Acc::default();
            for &(pr, ac, k, h) in &res {
                a.push(pr, ac);
                fa.push(pr, ac);
                kinds[k].push(pr, ac);
                if (17..20).contains(&h) {
                    eve.push(pr, ac);
                }
            }
            let s = fa.stats().unwrap_or_default();
            per_fold.push(format!("{:.2} ({:+.2})", s.1 / 60.0, s.5 / 60.0));
        }
        let mae = |x: &Acc| x.stats().map_or("    -".to_string(), |s| format!("{:5.2}", s.1 / 60.0));
        let (s, e) = (a.stats().unwrap_or_default(), eve.stats().unwrap_or_default());
        text += &format!(
            "{name:<34} {:5.2} {:6.2} {:>+6.2}  | {} {} {} {} | {:5.2} {:>+6.2} | {}\n",
            s.1 / 60.0,
            s.2,
            s.5 / 60.0,
            mae(&kinds[0]),
            mae(&kinds[1]),
            mae(&kinds[2]),
            mae(&kinds[3]),
            e.1 / 60.0,
            e.5 / 60.0,
            per_fold.join("  ")
        );
    }
    text
}

/// Compare live-correction settings by replay, with the same folds as `tune`.
/// For each fold the model is fitted on the other days; then, every five
/// minutes of the held-out days, each variant builds its live view from the
/// traversals known by then (allowing for the feed's delay) and predicts the
/// chains that start in those five minutes.
///
/// `ahead_s` tests trips that leave later: the live view is the one from
/// `ahead_s` before the chain starts, as if the rider had asked that early.
pub fn tune_live(net: &Network, rows: &[Row], folds: &[(i64, i64)], data_until: i64, mp: &Params, variants: &[(String, LiveParams)], ahead_s: i64) -> String {
    let in_fold = |t: i64, f: &(i64, i64)| t >= f.0 && t < f.1;
    let nv = variants.len();
    // Look back as far as the longest window among the variants.
    let horizon = variants.iter().map(|v| v.1.window_s as i64).max().unwrap_or(WINDOW_S);
    // Per variant: (predicted, actual, start hour) for every chain; index 0 of the outer pair is history only.
    let mut hist = Acc::default();
    let mut acc: Vec<Acc> = (0..nv).map(|_| Acc::default()).collect();
    let mut eve: Vec<Acc> = (0..nv).map(|_| Acc::default()).collect();
    let mut long: Vec<Acc> = (0..nv).map(|_| Acc::default()).collect();
    let mut text = String::new();
    for f in folds {
        let train: Vec<Obs> = rows.iter().filter(|r| r.obs.t_enter < data_until && !in_fold(r.obs.t_enter, f)).map(|r| r.obs).collect();
        let model = Arc::new(Model::fit_with(net, &train, mp));
        let valid: Vec<Row> = rows.iter().filter(|r| in_fold(r.obs.t_enter, f)).copied().collect();
        let all = chains(&valid);
        let mut sel: Vec<&Chain> = all.iter().filter(|c| c.rows.len() >= 5).collect();
        sel.sort_by_key(|c| c.rows[0].obs.t_enter);
        // Traversals that can be in some slot's window: (finished, segment, observed / usual).
        let mut known: Vec<(f64, u32, f32)> = rows
            .par_iter()
            .filter(|r| r.obs.t_enter >= f.0 - ahead_s - horizon - 900 && r.obs.t_enter < f.1 && net.segments[r.obs.seg as usize].len_m >= 30.0)
            .map(|r| {
                let at = Local::from_epoch(r.obs.t_enter);
                (r.t_enter + r.obs.dur as f64, r.obs.seg, r.obs.dur / model.secs(net, r.obs.seg, at.day_type(), at.tod))
            })
            .collect();
        known.sort_by(|a, b| a.0.total_cmp(&b.0));
        // Group chains into five-minute slots.
        let mut slots: Vec<(i64, usize, usize)> = Vec::new();
        let mut i = 0;
        while i < sel.len() {
            let slot = sel[i].rows[0].obs.t_enter.div_euclid(REPLAY_STEP_S) * REPLAY_STEP_S;
            let mut j = i;
            while j < sel.len() && sel[j].rows[0].obs.t_enter < slot + REPLAY_STEP_S {
                j += 1;
            }
            slots.push((slot, i, j));
            i = j;
        }
        // Per slot: history-only and per-variant predictions.
        let out: Vec<Vec<(f32, Vec<f32>, f32, u32, usize)>> = slots
            .par_iter()
            .map(|&(slot, i, j)| {
                let known_until = slot - ahead_s - FEED_LATENCY_S;
                let lo = known.partition_point(|x| x.0 < (known_until - horizon) as f64);
                let hi = known.partition_point(|x| x.0 <= known_until as f64);
                let blends: Vec<Blended> = variants
                    .iter()
                    .map(|(_, lp)| {
                        let items = known[lo..hi].iter().map(|&(done, seg, r)| (seg, (known_until as f64 - done) as f32, r));
                        Blended { base: model.clone(), live: Arc::new(LiveRatios::from_ratios(net, items, known_until, lp)) }
                    })
                    .collect();
                sel[i..j]
                    .iter()
                    .map(|c| {
                        let actual: f32 = c.rows.iter().map(|r| r.obs.dur).sum();
                        let h = Local::from_epoch(c.rows[0].obs.t_enter).tod / 3600;
                        (predict_chain(net, model.as_ref(), &c.rows), blends.iter().map(|b| predict_chain(net, b, &c.rows)).collect(), actual, h, c.rows.len())
                    })
                    .collect()
            })
            .collect();
        for (h0, preds, actual, h, hops) in out.into_iter().flatten() {
            hist.push(h0, actual);
            for (v, pr) in preds.into_iter().enumerate() {
                acc[v].push(pr, actual);
                if (17..20).contains(&h) {
                    eve[v].push(pr, actual);
                }
                if hops >= 15 {
                    long[v].push(pr, actual);
                }
            }
        }
        text += &format!("fold {} to {}: {} chains\n", Local::from_epoch(f.0).date_string(), Local::from_epoch(f.1 - 1).date_string(), sel.len());
    }
    let s = hist.stats().unwrap_or_default();
    text += &format!("trips leaving {} min after the live view was taken\n", ahead_s / 60);
    text += "MAE in minutes over all folds; evening = chains starting 17:00-20:00; long = chains of 15+ hops\n";
    text += &format!("{:<40} {:>5} {:>6} {:>6} | {:>5} {:>6} | {:>5} {:>6}\n", "variant", "MAE", "MAPE%", "bias", "eve", "bias", "long", "bias");
    text += &format!("{:<40} {:5.2} {:6.2} {:>+6.2} |\n", "history only", s.1 / 60.0, s.2, s.5 / 60.0);
    for (v, (name, _)) in variants.iter().enumerate() {
        let (s, e, l) = (acc[v].stats().unwrap_or_default(), eve[v].stats().unwrap_or_default(), long[v].stats().unwrap_or_default());
        text += &format!(
            "{name:<40} {:5.2} {:6.2} {:>+6.2} | {:5.2} {:>+6.2} | {:5.2} {:>+6.2}\n",
            s.1 / 60.0,
            s.2,
            s.5 / 60.0,
            e.1 / 60.0,
            e.5 / 60.0,
            l.1 / 60.0,
            l.5 / 60.0
        );
    }
    text
}

/// Where does the remaining error come from? Fits one model and reports what
/// would be gained from knowing things it does not: how the whole city ran
/// that day (an upper bound on what live data can add), and per-route or
/// per-vehicle habits learned from the training period. Development tool.
pub fn explore(net: &Network, rows: &[Row], train_until: i64, valid_until: i64, p: &Params) -> String {
    let train: Vec<&Row> = rows.iter().filter(|r| r.obs.t_enter < train_until).collect();
    let valid: Vec<Row> = rows.iter().filter(|r| r.obs.t_enter >= train_until && r.obs.t_enter < valid_until).copied().collect();
    let model = Model::fit_with(net, &train.iter().map(|r| r.obs).collect::<Vec<_>>(), p);
    let pred_row = |r: &Row| {
        let t = Local::from_epoch(r.obs.t_enter);
        model.secs(net, r.obs.seg, t.day_type(), t.tod)
    };
    // Training residuals by route and by vehicle: (sum actual, sum predicted).
    let n_veh = rows.iter().map(|r| r.vehicle).max().unwrap_or(0) as usize + 1;
    let fold = |key: &(dyn Fn(&Row) -> usize + Sync), n: usize| -> Vec<(f64, f64)> {
        train
            .par_iter()
            .fold(|| vec![(0.0, 0.0); n], |mut a, r| {
                let k = key(r);
                a[k].0 += r.obs.dur as f64;
                a[k].1 += pred_row(r) as f64;
                a
            })
            .reduce(|| vec![(0.0, 0.0); n], |mut a, b| {
                for (x, y) in a.iter_mut().zip(b) {
                    x.0 += y.0;
                    x.1 += y.1;
                }
                a
            })
    };
    let by_route = fold(&|r: &Row| r.route as usize, net.routes.len());
    let by_veh = fold(&|r: &Row| r.vehicle as usize, n_veh);
    let (ta, tp) = by_route.iter().fold((0.0, 0.0), |a, x| (a.0 + x.0, a.1 + x.1));
    let global = ta / tp;
    // Shrink each factor toward the global one with K seconds of pseudo-data.
    let factor = |v: &[(f64, f64)], k: f64| -> Vec<f32> { v.iter().map(|&(a, pr)| ((a + k * global) / (pr + k) / global) as f32).collect() };

    let all = chains(&valid);
    let sel: Vec<&Chain> = all.iter().filter(|c| c.rows.len() >= 5).collect();
    // (predicted, actual, day, hour, route, vehicle)
    let res: Vec<(f32, f32, i64, u32, usize, usize)> = sel
        .par_iter()
        .map(|c| {
            let t = Local::from_epoch(c.rows[0].obs.t_enter);
            (predict_chain(net, &model, &c.rows), c.rows.iter().map(|r| r.obs.dur).sum(), t.day, t.tod / 3600, c.rows[0].route as usize, c.rows[0].vehicle as usize)
        })
        .collect();
    let mut text = format!("{} validation chains; training actual/predicted overall {:.3}\n", res.len(), global);
    let mut show = |name: &str, f: &dyn Fn(&(f32, f32, i64, u32, usize, usize)) -> f32| {
        let mut a = Acc::default();
        for r in &res {
            a.push(r.0 * f(r), r.1);
        }
        text += &line(name, &a, 60.0, "m");
        text.push('\n');
    };
    show("model", &|_| 1.0);
    // Oracles: the true city-wide ratio of that day, and of that day and hour.
    let mut day: HashMap<i64, (f64, f64)> = HashMap::new();
    let mut day_hour: HashMap<(i64, u32), (f64, f64)> = HashMap::new();
    for r in &res {
        for e in [day.entry(r.2).or_default(), day_hour.entry((r.2, r.3)).or_default()] {
            e.0 += r.1 as f64;
            e.1 += r.0 as f64;
        }
    }
    show("x true ratio of the day (oracle)", &|r| (day[&r.2].0 / day[&r.2].1) as f32);
    show("x true ratio of day+hour (oracle)", &|r| (day_hour[&(r.2, r.3)].0 / day_hour[&(r.2, r.3)].1) as f32);
    for k in [3_000.0, 30_000.0, 300_000.0] {
        let (fr, fv) = (factor(&by_route, k), factor(&by_veh, k));
        show(&format!("x route habit (k={k})"), &|r| fr[r.4]);
        show(&format!("x vehicle habit (k={k})"), &|r| fv[r.5]);
        show(&format!("x route x vehicle (k={k})"), &|r| fr[r.4] * fv[r.5]);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(run: u32, k: u16, t: f64, dur: f32) -> Row {
        Row { obs: Obs { seg: k as u32, t_enter: t as i64, dur }, t_enter: t, k, route: 0, vehicle: run, run }
    }

    #[test]
    fn chains_break_on_missing_hop_and_on_time_gap() {
        let rows = vec![
            row(0, 0, 0.0, 50.0),
            row(0, 1, 50.0, 60.0),
            row(0, 2, 110.0, 40.0),
            // hop 3 missing
            row(0, 4, 300.0, 30.0),
            row(0, 5, 330.0, 30.0),
            // same positions later in the day would be another pass: time does not join
            row(1, 0, 0.0, 50.0),
            row(1, 1, 500.0, 50.0),
        ];
        let mut lens: Vec<usize> = chains(&rows).iter().map(|c| c.rows.len()).collect();
        lens.sort();
        assert_eq!(lens, vec![1, 1, 2, 3]);
    }

    #[test]
    fn error_stats() {
        let mut a = Acc::default();
        a.push(110.0, 100.0);
        a.push(80.0, 100.0);
        let (n, mae, mape, _, _, bias) = a.stats().unwrap();
        assert_eq!(n, 2);
        assert!((mae - 15.0).abs() < 1e-4 && (mape - 15.0).abs() < 1e-4 && (bias + 5.0).abs() < 1e-4);
    }
}
