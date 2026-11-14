//! Validation pairs and the Google Maps side of the comparison.
//!
//! Google's Routes API accepts transit departure times at most 7 days in the
//! past, so `busplan google` has to be run regularly while the feed archive
//! grows; it cannot be done in one batch at the end.
//!
//! Raw responses are kept under data/google/ (git-ignored) for our own
//! analysis only. Google's terms restrict storing and redistributing its
//! content: do not commit or publish that directory. The validation report
//! quotes per-pair numbers, nothing more.

use crate::clock::{Local, civil_from_days, now_epoch};
use crate::geom::dist;
use crate::planner::{Engine, LegOut, plan};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

/// Fields beyond id and coordinates are for the comparison report.
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct Pair {
    pub id: String,
    pub from: String,
    pub from_ll: (f64, f64),
    pub to: String,
    pub to_ll: (f64, f64),
    pub crow_km: f64,
    /// "direct" or "one_change" under the timetable at build time.
    pub kind: String,
    pub routes: String,
}

pub const PAIRS_HEADER: &str = "id,from,from_lat,from_lon,to,to_lat,to_lon,crow_km,kind,routes,trips_per_day";

/// Build a stratified set of origin-destination pairs from a list of hub
/// stop names: direct and one-change journeys across three distance bands.
///
/// Direct pairs are preferred where the route is frequent, because a direct
/// frequent route is what gives us observed bus trips to compare against.
pub fn build_pairs(eng: &Engine, hubs: &[String], per_bucket_direct: usize, per_bucket_change: usize) -> Result<String> {
    let net = &eng.net;
    let mut places = Vec::new();
    for h in hubs {
        match net.find_place(h) {
            Some(p) => places.push(p),
            None => eprintln!("hub not found, skipped: {h}"),
        }
    }
    // Deterministic pseudo-random order so the set does not favour the top of the hub list.
    let mut order: Vec<(usize, usize)> =
        (0..places.len()).flat_map(|i| (0..places.len()).filter(move |&j| j != i).map(move |j| (i, j))).collect();
    order.sort_by_key(|&(i, j)| (i as u64 * 2_654_435_761 + j as u64 * 40_503) % 1_000_003);

    let depart = Local::parse("2026-11-17T09:00").unwrap();
    let mut taken: HashMap<(usize, &str), usize> = HashMap::new();
    let mut as_origin: HashMap<usize, usize> = HashMap::new();
    let mut as_dest: HashMap<usize, usize> = HashMap::new();
    let mut rows: Vec<String> = Vec::new();
    let mut add = |i: usize, j: usize, force: bool, rows: &mut Vec<String>| -> Result<()> {
        let (a, b) = (&places[i], &places[j]);
        let km = dist(a.xy, b.xy) / 1000.0;
        if !force && (km < 3.0 || as_origin.get(&i).copied().unwrap_or(0) >= 3 || as_dest.get(&j).copied().unwrap_or(0) >= 3) {
            return Ok(());
        }
        let (la, lb) = (crate::geom::to_ll(a.xy), crate::geom::to_ll(b.xy));
        let (fa, fb) = (format!("{:.6},{:.6}", la.0, la.1), format!("{:.6},{:.6}", lb.0, lb.1));
        let Ok(resp) = plan(eng, &fa, &fb, depart, 1) else { return Ok(()) };
        let Some(opt) = resp.options.iter().find(|o| o.ride_s > 0) else { return Ok(()) };
        let buses: Vec<&LegOut> = opt.legs.iter().filter(|l| matches!(l, LegOut::Bus { .. })).collect();
        let kind = match buses.len() {
            1 => "direct",
            2 => "one_change",
            _ => return Ok(()),
        };
        // Trips per day of the least frequent route used.
        let trips = buses
            .iter()
            .filter_map(|l| if let LegOut::Bus { route_id, .. } = l { net.route_index.get(route_id) } else { None })
            .map(|&r| net.routes[r as usize].starts.len())
            .min()
            .unwrap_or(0);
        let band = if km < 8.0 { 0 } else if km < 16.0 { 1 } else { 2 };
        let cap = if kind == "direct" { per_bucket_direct } else { per_bucket_change };
        let n = taken.entry((band, kind)).or_default();
        if !force && (*n >= cap || (kind == "direct" && trips < 30)) {
            return Ok(());
        }
        *n += 1;
        *as_origin.entry(i).or_default() += 1;
        *as_dest.entry(j).or_default() += 1;
        rows.push(format!(
            "p{:02},{},{:.6},{:.6},{},{:.6},{:.6},{:.1},{},{},{}",
            rows.len() + 1,
            a.label.replace(',', " "),
            la.0,
            la.1,
            b.label.replace(',', " "),
            lb.0,
            lb.1,
            km,
            kind,
            opt.summary.replace(" → ", "|"),
            trips
        ));
        Ok(())
    };
    // The brief's own example always comes first.
    if let (Some(i), Some(j)) = (
        places.iter().position(|p| p.label.to_lowercase().contains("isbt kashmere gate")),
        places.iter().position(|p| p.label.to_lowercase() == "nehru place"),
    ) {
        add(i, j, true, &mut rows)?;
    }
    for (i, j) in order {
        add(i, j, false, &mut rows)?;
    }
    Ok(format!("{PAIRS_HEADER}\n{}\n", rows.join("\n")))
}

pub fn load_pairs(path: &Path) -> Result<Vec<Pair>> {
    let mut rd = csv::Reader::from_path(path).with_context(|| format!("opening {}", path.display()))?;
    let mut out = Vec::new();
    for r in rd.records() {
        let r = r?;
        out.push(Pair {
            id: r[0].to_string(),
            from: r[1].to_string(),
            from_ll: (r[2].parse()?, r[3].parse()?),
            to: r[4].to_string(),
            to_ll: (r[5].parse()?, r[6].parse()?),
            crow_km: r[7].parse()?,
            kind: r[8].to_string(),
            routes: r[9].to_string(),
        });
    }
    Ok(out)
}

fn rfc3339_utc(epoch: i64) -> String {
    let (d, s) = (epoch.div_euclid(86_400), epoch.rem_euclid(86_400));
    let (y, m, dd) = civil_from_days(d);
    format!("{y:04}-{m:02}-{dd:02}T{:02}:{:02}:{:02}Z", s / 3600, (s / 60) % 60, s % 60)
}

/// "3345s" -> 3345.
fn secs(v: &Value) -> f64 {
    v.as_str().and_then(|s| s.trim_end_matches('s').parse().ok()).unwrap_or(0.0)
}

/// One route option from a Google response, reduced to what we compare.
#[derive(Debug, Clone, PartialEq)]
pub struct GRoute {
    pub total_s: f64,
    pub walk_s: f64,
    /// Time on vehicles.
    pub transit_s: f64,
    pub n_transit: usize,
    /// Every vehicle leg is a bus.
    pub bus_only: bool,
    pub lines: String,
    pub vehicles: String,
    pub depart: String,
    pub arrive: String,
}

pub fn summarise(resp: &Value) -> Vec<GRoute> {
    let mut out = Vec::new();
    for r in resp["routes"].as_array().map(|v| v.as_slice()).unwrap_or(&[]) {
        let mut g = GRoute {
            total_s: secs(&r["duration"]),
            walk_s: 0.0,
            transit_s: 0.0,
            n_transit: 0,
            bus_only: true,
            lines: String::new(),
            vehicles: String::new(),
            depart: String::new(),
            arrive: String::new(),
        };
        let (mut lines, mut vehicles) = (Vec::new(), Vec::new());
        for leg in r["legs"].as_array().map(|v| v.as_slice()).unwrap_or(&[]) {
            for st in leg["steps"].as_array().map(|v| v.as_slice()).unwrap_or(&[]) {
                let mut d = secs(&st["staticDuration"]);
                if st["travelMode"] == "TRANSIT" {
                    let td = &st["transitDetails"];
                    // Where an agency supplies live predictions they show up in the
                    // stop times, not in staticDuration (measured: London, Sydney).
                    let when = |k: &str| td["stopDetails"][k].as_str().and_then(Local::parse).map(|l| l.epoch());
                    if let (Some(a), Some(b)) = (when("departureTime"), when("arrivalTime")) {
                        if b > a {
                            d = (b - a) as f64;
                        }
                    }
                    let line = &td["transitLine"];
                    let vt = line["vehicle"]["type"].as_str().unwrap_or("?");
                    g.transit_s += d;
                    g.n_transit += 1;
                    g.bus_only &= vt == "BUS";
                    lines.push(line["nameShort"].as_str().or(line["name"].as_str()).unwrap_or("?").to_string());
                    vehicles.push(vt.to_string());
                    let sd = &td["stopDetails"];
                    if g.depart.is_empty() {
                        g.depart = sd["departureTime"].as_str().unwrap_or("").to_string();
                    }
                    g.arrive = sd["arrivalTime"].as_str().unwrap_or("").to_string();
                } else {
                    g.walk_s += d;
                }
            }
        }
        g.bus_only &= g.n_transit > 0;
        g.lines = lines.join("|");
        g.vehicles = vehicles.join("|");
        out.push(g);
    }
    out
}

/// Hard limit on Google calls per calendar month, enforced here before any
/// request is sent. The user must never be billed for this.
///
/// Google's India pricing (checked 2026-11-13) gives 70,000 free Compute
/// Routes Essentials requests a month; a TRANSIT request with our options
/// uses no Pro or Enterprise feature, so it is Essentials. Even the smallest
/// free allowance of any Compute Routes tier (Enterprise, 7,000) is more than
/// three times this cap. The whole validation needs about 1,300 calls.
pub const MONTHLY_CALL_CAP: u32 = 2_000;

/// Calls sent so far in `month` ("2026-11"), from data/google/usage.txt.
pub fn calls_this_month(out_dir: &Path, month: &str) -> u32 {
    std::fs::read_to_string(out_dir.join("usage.txt"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once(' '))
        .find(|(m, _)| *m == month)
        .and_then(|(_, n)| n.trim().parse().ok())
        .unwrap_or(0)
}

/// Count one call against the monthly cap, or refuse. Recorded *before* the
/// request goes out, so a crash can only over-count.
fn reserve_call(out_dir: &Path, month: &str) -> Result<u32> {
    let used = calls_this_month(out_dir, month);
    if used >= MONTHLY_CALL_CAP {
        bail!("monthly cap reached: {used} Google calls sent in {month}, limit {MONTHLY_CALL_CAP}. Not sending more, to stay far inside the free allowance.");
    }
    let path = out_dir.join("usage.txt");
    let mut lines: Vec<String> =
        std::fs::read_to_string(&path).unwrap_or_default().lines().filter(|l| !l.starts_with(month)).map(String::from).collect();
    lines.push(format!("{month} {}", used + 1));
    std::fs::write(&path, lines.join("\n") + "\n")?;
    Ok(used + 1)
}

const FIELD_MASK: &str = "routes.duration,routes.staticDuration,routes.distanceMeters,routes.legs.steps.travelMode,routes.legs.steps.staticDuration,routes.legs.steps.distanceMeters,routes.legs.steps.transitDetails";

fn query(agent: &ureq::Agent, key: &str, p: &Pair, depart: Local) -> Result<(u16, String)> {
    let body = json!({
        "origin": {"location": {"latLng": {"latitude": p.from_ll.0, "longitude": p.from_ll.1}}},
        "destination": {"location": {"latLng": {"latitude": p.to_ll.0, "longitude": p.to_ll.1}}},
        "travelMode": "TRANSIT",
        "departureTime": rfc3339_utc(depart.epoch()),
        "computeAlternativeRoutes": true,
        // A preference, not a filter: rail legs can still come back and are
        // flagged by `bus_only` in the summary.
        "transitPreferences": {"allowedTravelModes": ["BUS"]},
    });
    let mut res = agent
        .post("https://routes.googleapis.com/directions/v2:computeRoutes")
        .header("X-Goog-Api-Key", key)
        .header("X-Goog-FieldMask", FIELD_MASK)
        .send_json(&body)?;
    Ok((res.status().as_u16(), res.body_mut().read_to_string()?))
}

pub struct FetchStats {
    pub fetched: usize,
    pub skipped: usize,
    pub with_bus_only: usize,
    pub no_route: usize,
}

/// Ask Google for every pair at one departure time. Already-fetched pairs
/// are skipped, so this is safe to re-run.
pub fn fetch(key: &str, pairs: &[Pair], depart: Local, out_dir: &Path, force: bool) -> Result<FetchStats> {
    let age = now_epoch() - depart.epoch();
    if age > 7 * 86_400 {
        bail!("{} is more than 7 days ago; Google does not accept it", depart.iso());
    }
    let slot = depart.iso()[..16].replace(':', "");
    let raw = out_dir.join("raw").join(&slot);
    std::fs::create_dir_all(&raw)?;
    let results = out_dir.join("results.csv");
    let new = !results.exists();
    let mut csv = std::fs::OpenOptions::new().create(true).append(true).open(&results)?;
    if new {
        writeln!(csv, "queried_at,depart,pair_id,option,total_s,walk_s,transit_s,n_transit,bus_only,lines,vehicles,first_departure,last_arrival")?;
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(std::time::Duration::from_secs(30)))
        .build()
        .into();
    let month = Local::from_epoch(now_epoch()).date_string()[..7].to_string();
    let mut st = FetchStats { fetched: 0, skipped: 0, with_bus_only: 0, no_route: 0 };
    for p in pairs {
        let file = raw.join(format!("{}.json", p.id));
        if file.exists() && !force {
            st.skipped += 1;
            continue;
        }
        // Google answers a burst of calls with a short-lived 403 or 429 (seen
        // after ~45 calls in 15 s); it clears within half a minute.
        reserve_call(out_dir, &month)?;
        let (mut status, mut text) = query(&agent, key, p, depart)?;
        for wait in [20, 40, 80] {
            if !matches!(status, 403 | 429 | 500..=599) {
                break;
            }
            eprintln!("Google returned HTTP {status} for {}; retrying in {wait} s", p.id);
            std::thread::sleep(std::time::Duration::from_secs(wait));
            reserve_call(out_dir, &month)?; // retries count against the cap too
            (status, text) = query(&agent, key, p, depart)?;
        }
        if status != 200 {
            let msg = serde_json::from_str::<Value>(&text).ok().and_then(|v| v["error"]["message"].as_str().map(String::from)).unwrap_or(text);
            bail!("Google returned HTTP {status} for pair {}: {msg}", p.id);
        }
        std::fs::write(&file, &text)?;
        let v: Value = serde_json::from_str(&text).context("Google response was not JSON")?;
        let routes = summarise(&v);
        if routes.is_empty() {
            st.no_route += 1;
        }
        if routes.iter().any(|r| r.bus_only) {
            st.with_bus_only += 1;
        }
        let now = Local::from_epoch(now_epoch()).iso();
        for (i, r) in routes.iter().enumerate() {
            writeln!(
                csv,
                "{now},{},{},{i},{:.0},{:.0},{:.0},{},{},{},{},{},{}",
                depart.iso(),
                p.id,
                r.total_s,
                r.walk_s,
                r.transit_s,
                r.n_transit,
                r.bus_only,
                r.lines.replace(',', " "),
                r.vehicles,
                r.depart,
                r.arrive
            )?;
        }
        st.fetched += 1;
        std::thread::sleep(std::time::Duration::from_millis(600));
    }
    Ok(st)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monthly_cap_refuses_before_sending() {
        let dir = std::env::temp_dir().join(format!("busplan-cap-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("usage.txt"), format!("2026-10 5\n2026-11 {}\n", MONTHLY_CALL_CAP - 1)).unwrap();
        assert_eq!(calls_this_month(&dir, "2026-11"), MONTHLY_CALL_CAP - 1);
        assert_eq!(reserve_call(&dir, "2026-11").unwrap(), MONTHLY_CALL_CAP);
        assert!(reserve_call(&dir, "2026-11").is_err(), "the call after the cap must be refused");
        assert_eq!(calls_this_month(&dir, "2026-11"), MONTHLY_CALL_CAP, "a refused call is not counted");
        // Other months are untouched, and a new month starts from zero.
        assert_eq!(calls_this_month(&dir, "2026-10"), 5);
        assert_eq!(reserve_call(&dir, "2026-12").unwrap(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn utc_timestamp_for_google() {
        // 08:30 IST on 17 Nov 2026 is 03:00 UTC.
        let l = Local::parse("2026-11-17T08:30").unwrap();
        assert_eq!(rfc3339_utc(l.epoch()), "2026-11-17T03:00:00Z");
        // Early morning IST falls on the previous UTC date.
        assert_eq!(rfc3339_utc(Local::parse("2026-11-17T02:00").unwrap().epoch()), "2026-11-16T20:30:00Z");
    }

    #[test]
    fn summarises_a_routes_api_response() {
        // Shape per the Routes API reference; values invented for the test.
        let v = json!({"routes": [{
            "duration": "3000s",
            "legs": [{"steps": [
                {"travelMode": "WALK", "staticDuration": "200s"},
                {"travelMode": "TRANSIT", "staticDuration": "2400s", "transitDetails": {
                    "stopDetails": {"departureTime": "2026-11-17T03:10:00Z", "arrivalTime": "2026-11-17T03:50:00Z"},
                    "transitLine": {"nameShort": "425", "vehicle": {"type": "BUS"}}}},
                {"travelMode": "WALK", "staticDuration": "100s"}
            ]}]
        }, {
            "duration": "2000s",
            "legs": [{"steps": [
                {"travelMode": "TRANSIT", "staticDuration": "1500s", "transitDetails": {
                    "transitLine": {"name": "Violet Line", "vehicle": {"type": "SUBWAY"}}}}
            ]}]
        }]});
        let r = summarise(&v);
        assert_eq!(r.len(), 2);
        // Ride time comes from the stop times (03:10 to 03:50), not staticDuration.
        assert_eq!((r[0].total_s, r[0].walk_s, r[0].transit_s, r[0].n_transit), (3000.0, 300.0, 2400.0, 1));
        assert!(r[0].bus_only && r[0].lines == "425" && r[0].depart.ends_with("03:10:00Z"));
        assert!(!r[1].bus_only && r[1].vehicles == "SUBWAY");
    }
}
