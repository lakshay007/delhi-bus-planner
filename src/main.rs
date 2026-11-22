//! busplan — Delhi bus travel-time model and journey planner.
//!
//!   busplan info                     load the GTFS and print what was built
//!   busplan extract                  archived GPS -> segment traversals
//!   busplan model                    fit the model and print what it learned
//!   busplan eval                     backtest against held-out observed trips
//!   busplan plan <from> <to>         one query on the command line
//!   busplan pairs / busplan google   validation pairs and Google Maps times for them
//!   busplan compare                  ours vs Google vs observed buses, per pair and time
//!   busplan departures               do buses leave terminals on timetable?
//!   busplan save-model               write the fitted model to a file
//!   busplan serve                    HTTP API + web page
//!
//! See README.md for how to run it and TECHNICAL_NOTE.pdf for how it works.

mod api;
mod clock;
mod collect;
mod compare;
mod eval;
mod geom;
mod google;
mod gtfs;
mod link;
mod live;
mod model;
mod nextbus;
mod planner;
mod raptor;
mod rt;
mod validate;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use clock::Local;
use gtfs::Network;
use raptor::{Scheduled, SegTimes};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

#[derive(Parser)]
#[command(version, about = "Delhi bus travel-time model and journey planner")]
struct Cli {
    /// Directory holding the extracted static GTFS.
    #[arg(long, default_value = "data/gtfs", global = true)]
    gtfs: PathBuf,
    /// Directory holding rt_raw/ (archived feed) and traversals/.
    #[arg(long, default_value = "data", global = true)]
    data: PathBuf,
    /// A fitted model written by `busplan save-model`. It is used when there
    /// are no traversals to fit on, so a server can start without the data.
    #[arg(long, default_value = "model/busplan-model.gz", global = true)]
    model: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Load the GTFS and print a summary of the network.
    Info,
    /// Turn archived feed snapshots into segment traversals (data/traversals/<date>.csv).
    Extract {
        /// One IST date (YYYY-MM-DD). Default: every archived date not yet extracted, plus today.
        #[arg(long)]
        date: Option<String>,
        /// Re-extract dates that already have output.
        #[arg(long)]
        force: bool,
    },
    /// Turn the organisers' historical archive (one SQLite database per day,
    /// bus_movements_YYYY_MM_DD.db) into data/traversals/<date>.csv.
    Import {
        /// Directory holding the .db files.
        dir: PathBuf,
        /// Re-import dates that already have output.
        #[arg(long)]
        force: bool,
        /// Write the CSVs here instead of data/traversals.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Experiment: keep only polls about 25 s apart, as our own collector
        /// sees the feed. Use with --out; never mix the result into data/traversals.
        #[arg(long, requires = "out")]
        thin: bool,
    },
    /// Fit the travel-time model on all traversals and print what it learned.
    Model,
    /// Fit the model on all traversals and write it to the --model file.
    SaveModel,
    /// City-wide bus speed for each archived day, in four parts of the day.
    Days,
    /// Numbers for the validation report: ours, Google's and the timetable's
    /// estimates against real buses, on the bus legs Google proposed.
    Validate {
        #[arg(long, default_value = "validation/pairs.csv")]
        pairs: PathBuf,
        /// The unseen week starts here; the model for it is fitted on everything before.
        #[arg(long, default_value = "2026-10-25")]
        fit_until: String,
        /// ...and ends here (exclusive).
        #[arg(long, default_value = "2026-11-01")]
        test_until: String,
    },
    /// Compare model settings by cross-validation (development tool).
    Tune {
        /// Days held out in turn, as from:to pairs (Delhi dates; `to` is exclusive).
        #[arg(long, default_value = "2026-10-01:2026-10-09,2026-10-09:2026-10-17,2026-10-17:2026-10-25")]
        folds: String,
        /// Nothing from this date on is used, so the final backtest stays unseen.
        #[arg(long, default_value = "2026-10-25")]
        data_until: String,
        /// Which group to run: final, days, shrink (model settings); live
        /// (live correction); explore (where the remaining error comes from).
        #[arg(long, default_value = "final")]
        set: String,
        /// Live sets only: score trips that leave this many minutes after the live view was taken.
        #[arg(long, default_value_t = 0)]
        ahead_min: i64,
    },
    /// Backtest: fit on traversals before a cutoff, compare predictions with what buses did after it.
    Eval {
        /// Test on traversals from this moment on (YYYY-MM-DDTHH:MM, Delhi time).
        /// Default: the last archived day if there are several, else the last quarter of the data.
        #[arg(long)]
        test_from: Option<String>,
        /// Stop testing at this moment (same format). Default: the end of the data.
        #[arg(long)]
        test_until: Option<String>,
    },
    /// Replay test of live next-bus predictions: predicted against actual arrivals at stops.
    Nextbus {
        /// Replay from this moment on (YYYY-MM-DDTHH:MM, Delhi time); the model sees only earlier data.
        /// Default: the last archived day if there are several, else the midpoint of the data.
        #[arg(long)]
        test_from: Option<String>,
        /// Stop replaying at this moment (YYYY-MM-DDTHH:MM). Default: the end of the data.
        #[arg(long)]
        test_until: Option<String>,
    },
    /// Search stop names.
    Stops { query: String },
    /// Build the validation origin-destination set from a list of hub stop names.
    Pairs {
        /// One stop name per line.
        #[arg(long, default_value = "validation/hubs.txt")]
        hubs: PathBuf,
        #[arg(long, default_value = "validation/pairs.csv")]
        out: PathBuf,
    },
    /// Fetch Google Maps transit times for the validation pairs (needs GOOGLE_MAPS_API_KEY in .env).
    Google {
        /// Departure time(s), YYYY-MM-DDTHH:MM Delhi time. Repeatable. At most 7 days in the past.
        #[arg(long, required = true)]
        when: Vec<String>,
        #[arg(long, default_value = "validation/pairs.csv")]
        pairs: PathBuf,
        /// Only the first N pairs (for a trial run).
        #[arg(long)]
        limit: Option<usize>,
        /// Re-fetch pairs already stored for that time.
        #[arg(long)]
        force: bool,
    },
    /// Validation table: our estimate, Google's, and what real buses did, for every pair and fetched time.
    Compare {
        #[arg(long, default_value = "validation/pairs.csv")]
        pairs: PathBuf,
    },
    /// Do buses leave their terminals when the timetable says they do?
    Departures,
    /// Plan one journey and print it.
    Plan {
        /// Stop name or "lat,lon".
        from: String,
        /// Stop name or "lat,lon".
        to: String,
        /// YYYY-MM-DDTHH:MM in Delhi time. Default: now.
        #[arg(long)]
        depart: Option<String>,
        /// Travel times to use: auto, model, saved, or timetable.
        #[arg(long, default_value = "auto")]
        times: String,
        /// Print the API's JSON instead of text.
        #[arg(long)]
        json: bool,
        /// Maximum number of options.
        #[arg(long, default_value_t = 3)]
        max: usize,
    },
    /// Run the HTTP API and web page.
    Serve {
        #[arg(long, default_value_t = 8080)]
        port: u16,
        /// Travel times to use: auto, model, saved, or timetable. `auto`
        /// fits on data/traversals if there is any, else loads the --model
        /// file, else falls back to the timetable.
        #[arg(long, default_value = "auto")]
        times: String,
        /// Also archive the live feed into <data>/rt_raw while serving
        /// (needs TRANSPORTSTACK_API_KEY and TRANSPORTSTACK_FEED_URL in the
        /// environment or .env). Leave it off if a collector already runs.
        #[arg(long)]
        collect: bool,
    },
}

/// Below this many traversals the model is not trusted and `auto` uses the timetable.
const MIN_OBS_FOR_MODEL: usize = 50_000;

/// Model settings compared in `busplan tune`. The first
/// row is always the current default.
fn tune_variants(set: &str) -> Vec<(String, model::Params)> {
    use model::Params;
    let d = Params::default();
    let mut v: Vec<(String, Params)> = vec![("default".into(), d.clone())];
    let mut add = |name: &str, p: Params| v.push((name.to_string(), p));
    match set {
        "days" => {
            add("weekday+hol | sat+sun (old)", Params { day_map: [0, 1, 1, 0], ..d.clone() });
            add("weekday+hol | sat | sun", Params { day_map: [0, 1, 2, 0], ..d.clone() });
            add("weekday | sat | sun | hol", Params { day_map: [0, 1, 2, 3], ..d.clone() });
            add("weekday+sat | sun+hol", Params { day_map: [0, 0, 1, 1], ..d.clone() });
            add("weekday | sat+sun+hol", Params { day_map: [0, 1, 1, 1], ..d.clone() });
            add("one class", Params { day_map: [0, 0, 0, 0], ..d.clone() });
        }
        "shrink" => {
            add("plain average", Params { k_city: 0.0, k_seg: 0.0, k_bin: 0.0, cap: (0.0, 1e9), nb_w: 0.0, interp: false, ..d.clone() });
            add("plain average, capped", Params { k_city: 0.0, k_seg: 0.0, k_bin: 0.0, nb_w: 0.0, interp: false, ..d.clone() });
            add("no interpolation", Params { interp: false, ..d.clone() });
            for nb in [0.0, 1.0] {
                add(&format!("neighbour half-hours {nb}"), Params { nb_w: nb, ..d.clone() });
            }
            for k in [4.0, 64.0] {
                add(&format!("k_bin {k}"), Params { k_bin: k, ..d.clone() });
            }
            for k in [2.0, 32.0] {
                add(&format!("k_seg {k}"), Params { k_seg: k, ..d.clone() });
            }
        }
        // "final": caps, calibration and recency.
        _ => {
            for hi in [2.0, 4.0, 1e9] {
                add(&format!("cap {hi}"), Params { cap: (1.0 / hi, hi), ..d.clone() });
            }
            add("calibrate", Params { calibrate: true, ..d.clone() });
            for h in [7.0, 14.0, 28.0] {
                add(&format!("half-life {h} days"), Params { half_life_days: h, ..d.clone() });
            }
        }
    }
    v
}

/// Live-correction settings compared in `busplan tune --set live`.
fn live_variants() -> Vec<(String, live::LiveParams)> {
    use live::LiveParams;
    let d = LiveParams::default();
    let mut v: Vec<(String, LiveParams)> = vec![("default".into(), d.clone())];
    let mut add = |name: String, p: LiveParams| v.push((name, p));
    for t in [30.0, 60.0, 120.0, 240.0, 480.0, 960.0] {
        add(format!("lead tau {t} min"), LiveParams { tau_lead_s: t * 60.0, ..d.clone() });
    }
    for t in [7.5, 30.0, 60.0] {
        add(format!("age tau {t} min"), LiveParams { tau_age_s: t * 60.0, ..d.clone() });
    }
    add("window 30 min".into(), LiveParams { window_s: 1800.0, ..d.clone() });
    add("window 180 min, age tau 60".into(), LiveParams { window_s: 10_800.0, tau_age_s: 3600.0, ..d.clone() });
    for k in [1.0, 10.0] {
        add(format!("k_seg {k}"), LiveParams { k_seg: k, ..d.clone() });
    }
    for w in [0.0, 1.0] {
        add(format!("neighbour weight {w}"), LiveParams { neighbour_w: w, ..d.clone() });
    }
    add("cap 2".into(), LiveParams { cap: (0.5, 2.0), ..d.clone() });
    v
}

fn load_network(dir: &Path) -> Result<Network> {
    let t = Instant::now();
    let net = Network::load(dir)?;
    eprintln!("network: {} ({:.1}s)", net.summary(), t.elapsed().as_secs_f32());
    Ok(net)
}

fn fit(net: &Network, data: &Path) -> Result<model::Model> {
    let t = Instant::now();
    let obs = model::load_traversals(net, &data.join("traversals"), |_| true)?;
    let m = model::Model::fit(net, &obs);
    eprintln!(
        "model: {} traversals on {} of {} segments ({:.1}s)",
        m.n_obs,
        m.segments_with_data(),
        net.segments.len(),
        t.elapsed().as_secs_f32()
    );
    Ok(m)
}

fn has_traversals(data: &Path) -> bool {
    std::fs::read_dir(data.join("traversals")).is_ok_and(|d| d.flatten().any(|e| e.path().extension().is_some_and(|x| x == "csv")))
}

fn load_saved(net: &Network, path: &Path) -> Result<model::Model> {
    let t = Instant::now();
    let (m, matched) = model::Model::load(net, path)?;
    eprintln!("model: loaded {} ({} traversals, {:.1}s)", path.display(), m.n_obs, t.elapsed().as_secs_f32());
    if matched < net.segments.len() {
        eprintln!(
            "model: the file knows {matched} of this GTFS's {} segments; the rest use neighbouring segments or the city-wide pace. This GTFS is a different cut from the one the model was fitted on.",
            net.segments.len()
        );
    }
    Ok(m)
}

fn choose_times(net: &Network, data: &Path, saved: &Path, mode: &str) -> Result<(Arc<dyn SegTimes>, String)> {
    let describe = |m: &model::Model| {
        format!(
            "{} observed segment traversals, {} to {}, covering {} of {} segments",
            m.n_obs,
            Local::from_epoch(m.first_obs).iso(),
            Local::from_epoch(m.last_obs).iso(),
            m.segments_with_data(),
            net.segments.len()
        )
    };
    let from_file = || -> Result<(Arc<dyn SegTimes>, String)> {
        let m = load_saved(net, saved)?;
        let d = format!("{} (saved model {})", describe(&m), saved.display());
        Ok((Arc::new(m), d))
    };
    match mode {
        "timetable" => Ok((Arc::new(Scheduled), "published timetable (18 km/h everywhere)".into())),
        "model" => {
            let m = fit(net, data)?;
            let d = describe(&m);
            Ok((Arc::new(m), d))
        }
        "saved" => from_file(),
        "auto" => {
            // Whichever has seen more buses wins: a model fitted on the traversals
            // on disk, or the saved one. A few hours of fresh recordings must not
            // displace a saved model fitted on a month.
            let fitted = if has_traversals(data) { Some(fit(net, data)?) } else { None };
            let saved_model = if saved.exists() { Some(load_saved(net, saved)?) } else { None };
            let n_fit = fitted.as_ref().map_or(0, |m| m.n_obs);
            match (fitted, saved_model) {
                (Some(m), s) if m.n_obs >= MIN_OBS_FOR_MODEL && s.as_ref().is_none_or(|s| m.n_obs >= s.n_obs) => {
                    let d = describe(&m);
                    Ok((Arc::new(m), d))
                }
                (_, Some(s)) => {
                    if n_fit > 0 {
                        eprintln!("model: using the saved model ({} traversals) over the {n_fit} on disk; pass --times model to fit on those instead", s.n_obs);
                    }
                    let d = format!("{} (saved model {})", describe(&s), saved.display());
                    Ok((Arc::new(s), d))
                }
                (_, None) => {
                    eprintln!("only {n_fit} traversals (< {MIN_OBS_FOR_MODEL}) and no saved model at {}: using the timetable.", saved.display());
                    Ok((Arc::new(Scheduled), format!("published timetable (18 km/h everywhere); only {n_fit} traversals archived so far")))
                }
            }
        }
        other => bail!("--times must be auto, model, saved or timetable (got {other})"),
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Info => {
            let net = load_network(&cli.gtfs)?;
            let lens: Vec<f32> = net.segments.iter().map(|s| s.len_m).collect();
            let mains = || net.routes.iter().enumerate().filter(|(i, _)| net.is_main(*i as u32)).map(|(_, r)| r);
            let total_km: f64 = mains().map(|r| r.shape.len() / 1000.0).sum();
            let off: Vec<f64> = mains()
                .flat_map(|r| r.stops.iter().zip(&r.along).map(|(&s, _)| r.shape.nearest(net.stops[s as usize].xy).d))
                .collect();
            let far = off.iter().filter(|&&d| d > 150.0).count();
            println!("route shapes: {total_km:.0} km in total");
            println!("segment length m: min {:.0}, median {:.0}, max {:.0}", min(&lens), median(&lens), max(&lens));
            println!("stops further than 150 m from their route's shape: {far} of {}", off.len());
            println!("segments served by a single route: {} of {}", net.segments.iter().filter(|s| s.n_routes == 1).count(), net.segments.len());
            println!("routes with an opposite-direction sibling: {}", (0..net.routes.len() as u32).filter(|&r| net.sibling(r).is_some()).count());
        }
        Cmd::Extract { date, force } => {
            let net = load_network(&cli.gtfs)?;
            let (raw, out) = (cli.data.join("rt_raw"), cli.data.join("traversals"));
            let today = link::today();
            let dates = match date {
                Some(d) => vec![d],
                None => rt::list_dates(&raw)?,
            };
            if dates.is_empty() {
                bail!("no archived dates under {}", raw.display());
            }
            for d in dates {
                // A finished day never changes; today is still growing.
                if !force && d != today && out.join(format!("{d}.csv")).exists() {
                    println!("{d}: already extracted (use --force to redo)");
                    continue;
                }
                let t = Instant::now();
                let s = link::extract_date(&net, &raw, &out, &d)?;
                println!(
                    "{d}: {} snapshots, {} vehicles, {} fixes ({} on unknown routes) -> {} runs, {:.0}% of run fixes on-route, {} stop crossings, {} traversals ({} rejected for GPS gap, {} for speed; {} runs re-tagged to the opposite direction) in {:.1}s",
                    s.snapshots,
                    s.vehicles,
                    s.fixes,
                    s.fixes_unknown_route,
                    s.runs,
                    100.0 * s.fixes_on_route as f64 / s.fixes_in_runs.max(1) as f64,
                    s.crossings,
                    s.traversals,
                    s.rejected_gap,
                    s.rejected_speed,
                    s.relabelled,
                    t.elapsed().as_secs_f32()
                );
            }
        }
        Cmd::Import { dir, force, out, thin } => {
            let net = load_network(&cli.gtfs)?;
            let out = out.unwrap_or_else(|| cli.data.join("traversals"));
            let mut dbs: Vec<(String, PathBuf)> = std::fs::read_dir(&dir)?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter_map(|p| {
                    let d = p.file_name()?.to_str()?.strip_prefix("bus_movements_")?.strip_suffix(".db")?.replace('_', "-");
                    (d.len() == 10).then_some((d, p))
                })
                .collect();
            dbs.sort();
            if dbs.is_empty() {
                bail!("no bus_movements_YYYY_MM_DD.db files in {}", dir.display());
            }
            for (d, db) in dbs {
                if !force && out.join(format!("{d}.csv")).exists() {
                    println!("{d}: already imported (use --force to redo)");
                    continue;
                }
                let t = Instant::now();
                let s = link::extract_db(&net, &db, &out, &d, thin)?;
                println!(
                    "{d}: {} polls, {} vehicles, {} fixes ({} on unknown routes) -> {} runs, {:.0}% of run fixes on-route, {} stop crossings, {} traversals ({} rejected for GPS gap, {} for speed; {} runs re-tagged to the opposite direction) in {:.1}s",
                    s.snapshots,
                    s.vehicles,
                    s.fixes,
                    s.fixes_unknown_route,
                    s.runs,
                    100.0 * s.fixes_on_route as f64 / s.fixes_in_runs.max(1) as f64,
                    s.crossings,
                    s.traversals,
                    s.rejected_gap,
                    s.rejected_speed,
                    s.relabelled,
                    t.elapsed().as_secs_f32()
                );
            }
        }
        Cmd::Validate { pairs, fit_until, test_until } => {
            let ps = google::load_pairs(&pairs)?;
            let net = load_network(&cli.gtfs)?;
            let rows = eval::load_rows(&net, &cli.data.join("traversals"))?;
            let day = |d: &str| Local::parse(&format!("{d}T00:00")).map(|l| l.epoch()).with_context(|| format!("bad date {d}"));
            let out = validate::run(&net, &rows.rows, &ps, &cli.data.join("google"), day(&fit_until)?, day(&test_until)?)?;
            // Google's numbers stay out of the repository: these go next to its responses.
            let dir = cli.data.join("google").join("validation");
            std::fs::create_dir_all(&dir)?;
            std::fs::write(dir.join("legs.csv"), &out.legs_csv)?;
            std::fs::write(dir.join("by_hour.csv"), &out.hours_csv)?;
            std::fs::write(dir.join("pairs.csv"), &out.pairs_csv)?;
            print!("{}", out.text);
            println!("\ntables: {}", dir.display());
        }
        Cmd::Tune { folds, data_until, set, ahead_min } => {
            let net = load_network(&cli.gtfs)?;
            let rows = eval::load_rows(&net, &cli.data.join("traversals"))?;
            let day = |d: &str| Local::parse(&format!("{d}T00:00")).map(|l| l.epoch()).with_context(|| format!("bad date {d}"));
            let until = day(&data_until)?;
            let folds: Vec<(i64, i64)> = folds
                .split(',')
                .map(|f| {
                    let (a, b) = f.split_once(':').with_context(|| format!("bad fold {f}"))?;
                    Ok((day(a)?, day(b)?))
                })
                .collect::<Result<_>>()?;
            if set == "explore" {
                let f = folds.last().unwrap();
                print!("{}", eval::explore(&net, &rows.rows, f.0, f.1, &model::Params::default()));
            } else if set == "live" {
                print!("{}", eval::tune_live(&net, &rows.rows, &folds, until, &model::Params::default(), &live_variants(), ahead_min * 60));
            } else {
                print!("{}", eval::tune(&net, &rows.rows, &folds, until, &tune_variants(&set)));
            }
        }
        Cmd::Days => {
            let net = load_network(&cli.gtfs)?;
            let obs = model::load_traversals(&net, &cli.data.join("traversals"), |_| true)?;
            // Hours of the day: night, morning peak, midday, evening peak.
            let bands: [(&str, u32, u32); 4] = [("01-04", 1, 4), ("08-11", 8, 11), ("12-16", 12, 16), ("17-21", 17, 21)];
            // date -> (traversals, per band: metres, seconds)
            let mut days: std::collections::BTreeMap<i64, (usize, [(f64, f64); 4])> = Default::default();
            for o in &obs {
                let t = clock::Local::from_epoch(o.t_enter);
                let e = days.entry(t.day).or_default();
                e.0 += 1;
                let h = t.tod / 3600;
                if let Some(i) = bands.iter().position(|b| h >= b.1 && h < b.2) {
                    e.1[i].0 += net.segments[o.seg as usize].len_m as f64;
                    e.1[i].1 += o.dur as f64;
                }
            }
            println!("date        day  traversals   km/h at {}", bands.map(|b| b.0).join("   "));
            for (day, (n, b)) in &days {
                let t = clock::Local { day: *day, tod: 0 };
                let name = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"][t.weekday() as usize % 7];
                let cells: Vec<String> = b.iter().map(|&(m, s)| if s > 0.0 { format!("{:5.1}", 3.6 * m / s) } else { "    -".to_string() }).collect();
                println!("{}  {name}  {n:>10}           {}", t.date_string(), cells.join("   "));
            }
        }
        Cmd::Model => {
            let net = load_network(&cli.gtfs)?;
            let m = fit(&net, &cli.data)?;
            let classes = m.classes();
            println!("city-wide bus speed, km/h (observations)");
            println!("hour  {}", classes.iter().map(|c| format!("{:<22}", c.1)).collect::<String>());
            for h in 0..24 {
                let (a, b) = (2 * h, 2 * h + 1);
                let cells: String = classes
                    .iter()
                    .map(|&(c, _)| {
                        let (v, n) = (m.city_speed_kmh(c), m.city_count(c));
                        format!("{:5.1} ({:>8})      ", (v[a] + v[b]) / 2.0, n[a] + n[b])
                    })
                    .collect();
                println!("{h:02}    {cells}");
            }
            let mut by_n = [0usize; 4];
            for &n in &m.seg_n {
                by_n[match n { 0 => 0, 1..=7 => 1, 8..=99 => 2, _ => 3 }] += 1;
            }
            println!("segments by observations: none {}, 1-7 {}, 8-99 {}, 100+ {}", by_n[0], by_n[1], by_n[2], by_n[3]);
        }
        Cmd::SaveModel => {
            let net = load_network(&cli.gtfs)?;
            let m = fit(&net, &cli.data)?;
            m.save(&net, &cli.model)?;
            // Read it back and compare, so a file that is written is a file that works.
            let (l, _) = model::Model::load(&net, &cli.model)?;
            let mut checked = 0usize;
            for seg in 0..net.segments.len() as u32 {
                for day in 0..clock::DAY_KINDS {
                    for tod in (0..86_400).step_by(1800) {
                        if m.secs(&net, seg, day, tod) != l.secs(&net, seg, day, tod) || m.level(seg, day, tod) != l.level(seg, day, tod) {
                            bail!("the saved model does not reproduce the fitted one (segment {seg}); file not trustworthy");
                        }
                        checked += 1;
                    }
                }
            }
            println!(
                "wrote {} ({:.1} MB): {} traversals, {} to {}, {} of {} segments; {checked} estimates read back identical",
                cli.model.display(),
                std::fs::metadata(&cli.model)?.len() as f64 / 1e6,
                m.n_obs,
                Local::from_epoch(m.first_obs).iso(),
                Local::from_epoch(m.last_obs).iso(),
                m.segments_with_data(),
                net.segments.len()
            );
        }
        Cmd::Stops { query } => {
            let net = load_network(&cli.gtfs)?;
            for (name, lat, lon, routes) in net.search_names(&query, 25) {
                println!("{name}  ({lat:.5},{lon:.5})  {routes} route-stops");
            }
        }
        Cmd::Pairs { hubs, out } => {
            let net = load_network(&cli.gtfs)?;
            let eng = planner::Engine::new(net, Arc::new(Scheduled), "timetable".into());
            let names: Vec<String> = std::fs::read_to_string(&hubs)?
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .collect();
            let csv = google::build_pairs(&eng, &names, 6, 4)?;
            std::fs::write(&out, &csv)?;
            println!("{} pairs written to {}", csv.lines().count() - 1, out.display());
        }
        Cmd::Google { when, pairs, limit, force } => {
            let key = env_var("GOOGLE_MAPS_API_KEY")?;
            let gdir = cli.data.join("google");
            std::fs::create_dir_all(&gdir)?;
            let month = Local::from_epoch(clock::now_epoch()).date_string()[..7].to_string();
            let mut ps = google::load_pairs(&pairs)?;
            if let Some(n) = limit {
                ps.truncate(n);
            }
            for w in when {
                let depart = Local::parse(&w).ok_or_else(|| anyhow::anyhow!("could not read --when {w}"))?;
                let st = match google::fetch(&key, &ps, depart, &gdir, force) {
                    Ok(st) => st,
                    Err(e) => {
                        eprintln!("Google calls sent this month: {} of our cap of {}", google::calls_this_month(&gdir, &month), google::MONTHLY_CALL_CAP);
                        return Err(e);
                    }
                };
                println!(
                    "{}: fetched {}, already had {}, pairs with a bus-only option {}, pairs with no route {}",
                    depart.iso(),
                    st.fetched,
                    st.skipped,
                    st.with_bus_only,
                    st.no_route
                );
            }
            println!(
                "Google calls sent this month: {} of our cap of {} (Google's free allowance is 70,000)",
                google::calls_this_month(&gdir, &month),
                google::MONTHLY_CALL_CAP
            );
        }
        Cmd::Compare { pairs } => {
            let ps = google::load_pairs(&pairs)?;
            let g = compare::load_google(&cli.data.join("google").join("results.csv"))?;
            let net = load_network(&cli.gtfs)?;
            let rows = eval::load_rows(&net, &cli.data.join("traversals"))?;
            let gtfs = cli.gtfs.clone();
            let out = compare::run(&|| Network::load(&gtfs), &ps, &g, &rows.rows)?;
            let dir = cli.data.join("compare");
            std::fs::create_dir_all(&dir)?;
            std::fs::write(dir.join("compare.csv"), &out.csv)?;
            print!("{}", out.text);
            println!("\nfull table: {}", dir.join("compare.csv").display());
        }
        Cmd::Departures => {
            let net = load_network(&cli.gtfs)?;
            let rows = eval::load_rows(&net, &cli.data.join("traversals"))?;
            print!("{}", compare::departures(&net, &rows.rows));
        }
        Cmd::Nextbus { test_from, test_until } => {
            let net = load_network(&cli.gtfs)?;
            let rows = eval::load_rows(&net, &cli.data.join("traversals"))?;
            if rows.rows.is_empty() {
                bail!("no traversals; run `busplan extract` first");
            }
            let cutoff = match test_from {
                Some(s) => Local::parse(&s).ok_or_else(|| anyhow::anyhow!("could not read --test-from {s}"))?.epoch(),
                None => {
                    let mut ts: Vec<i64> = rows.rows.iter().map(|r| r.obs.t_enter).collect();
                    ts.sort_unstable();
                    let (first, last) = (Local::from_epoch(ts[0]), Local::from_epoch(*ts.last().unwrap()));
                    if last.day > first.day { Local { day: last.day, tod: 0 }.epoch() } else { ts[ts.len() / 2] }
                }
            };
            let until = match test_until {
                Some(s) => Local::parse(&s).ok_or_else(|| anyhow::anyhow!("could not read --test-until {s}"))?.epoch(),
                None => i64::MAX,
            };
            print!("{}", nextbus::replay(&net, &rows, &cli.data.join("rt_raw"), cutoff, until)?);
        }
        Cmd::Eval { test_from, test_until } => {
            let net = load_network(&cli.gtfs)?;
            let rows = eval::load_rows(&net, &cli.data.join("traversals"))?;
            if rows.rows.is_empty() {
                bail!("no traversals; run `busplan extract` first");
            }
            let cutoff = match test_from {
                Some(s) => Local::parse(&s).ok_or_else(|| anyhow::anyhow!("could not read --test-from {s}"))?.epoch(),
                None => {
                    let mut ts: Vec<i64> = rows.rows.iter().map(|r| r.obs.t_enter).collect();
                    ts.sort_unstable();
                    let (first, last) = (Local::from_epoch(ts[0]), Local::from_epoch(*ts.last().unwrap()));
                    if last.day > first.day {
                        Local { day: last.day, tod: 0 }.epoch()
                    } else {
                        ts[ts.len() * 3 / 4]
                    }
                }
            };
            let until = match test_until {
                Some(s) => Local::parse(&s).ok_or_else(|| anyhow::anyhow!("could not read --test-until {s}"))?.epoch(),
                None => i64::MAX,
            };
            println!("split at {}\n", Local::from_epoch(cutoff).iso());
            print!("{}", eval::backtest(&net, &rows.rows, cutoff, until).text);
        }
        Cmd::Plan { from, to, depart, times, json, max } => {
            let net = load_network(&cli.gtfs)?;
            let (t, basis) = choose_times(&net, &cli.data, &cli.model, &times)?;
            let eng = planner::Engine::new(net, t, basis);
            let depart = match depart {
                Some(s) => Local::parse(&s).ok_or_else(|| anyhow::anyhow!("could not read --depart {s}; use YYYY-MM-DDTHH:MM"))?,
                None => Local::from_epoch(clock::now_epoch()),
            };
            // Live data only matters for departures around now.
            if (depart.epoch() - clock::now_epoch()).abs() < planner::LIVE_HORIZON_S {
                match eng.refresh_live(&cli.data.join("rt_raw")) {
                    Ok(m) => eprintln!("{m}"),
                    Err(e) => eprintln!("live data not used: {e}"),
                }
            }
            let t0 = Instant::now();
            match planner::plan(&eng, &from, &to, depart, max) {
                Ok(r) => {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&r)?);
                    } else {
                        print!("{}", planner::describe(&r));
                        eprintln!("(query took {:.0} ms)", t0.elapsed().as_secs_f64() * 1000.0);
                    }
                }
                Err(e) => bail!("{e}"),
            }
        }
        Cmd::Serve { port, times, collect } => {
            let net = load_network(&cli.gtfs)?;
            if collect {
                let (key, url) = (env_var("TRANSPORTSTACK_API_KEY")?, env_var("TRANSPORTSTACK_FEED_URL")?);
                let raw = cli.data.join("rt_raw");
                std::fs::create_dir_all(&raw)?;
                eprintln!("feed: archiving into {} every {} s", raw.display(), collect::INTERVAL_S);
                std::thread::spawn(move || collect::run(raw, url, key));
            }
            let (t, basis) = choose_times(&net, &cli.data, &cli.model, &times)?;
            eprintln!("travel times: {basis}");
            let app = api::App { engine: planner::Engine::new(net, t, basis), raw_dir: cli.data.join("rt_raw") };
            tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(api::serve(app, port))?;
        }
    }
    Ok(())
}

/// A setting from the environment or the project's .env file.
fn env_var(name: &str) -> Result<String> {
    if let Ok(v) = std::env::var(name) {
        return Ok(v);
    }
    let text = std::fs::read_to_string(".env").unwrap_or_default();
    text.lines()
        .filter_map(|l| l.split_once('='))
        .find(|(k, _)| k.trim() == name)
        .map(|(_, v)| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .ok_or_else(|| anyhow::anyhow!("{name} is not set (environment or .env)"))
}

fn median(v: &[f32]) -> f32 {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.total_cmp(b));
    s.get(s.len() / 2).copied().unwrap_or(0.0)
}

fn min(v: &[f32]) -> f32 {
    v.iter().copied().fold(f32::INFINITY, f32::min)
}

fn max(v: &[f32]) -> f32 {
    v.iter().copied().fold(0.0, f32::max)
}
