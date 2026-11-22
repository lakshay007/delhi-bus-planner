//! Turn "from, to, depart" into journey options with walk, wait and ride
//! legs, ready to serialise as the API response.

use rayon::prelude::*;
use crate::clock::{Local, now_epoch};
use crate::geom::{P, dist, to_ll, to_xy};
use crate::gtfs::{Network, walk_secs};
use crate::live::{Blended, LiveRatios, Service, WINDOW_S};
use crate::raptor::{Journey, Leg, Search, SegTimes, Timetable};
use crate::{link, rt};
use serde::Serialize;
use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, RwLock};

/// Stops within this distance of the origin/destination are candidates.
pub const ACCESS_RADIUS_M: f64 = 800.0;
/// If nothing is that close, look this far and take the nearest few.
pub const ACCESS_FALLBACK_M: f64 = 2500.0;
/// Offer a walk-only option below this street distance.
pub const WALK_ONLY_M: f64 = 2000.0;
pub const MAX_RIDES: usize = 3;
/// How many seconds later a journey may arrive and still be preferred over
/// one with an extra change of bus.
pub const TRANSFER_PENALTY_S: u32 = 300;

/// Live view is used for departures from 15 minutes ago to this far ahead.
/// Replay on the archive shows live data still improves estimates for trips
/// leaving 4 hours later, and no longer at 6.
pub const LIVE_HORIZON_S: i64 = 4 * 3600;
/// A live view older than this is not used.
pub const LIVE_MAX_AGE_S: i64 = 300;
/// The live correction to travel times is held back until this much of the
/// feed has been recorded. In the first minutes only quick passes have had
/// time to finish, so the correction would read too fast.
pub const LIVE_MIN_RECORDED_S: i64 = 20 * 60;
/// "Buses seen in the last hour" is only reported once nearly the whole hour was recorded.
pub const SERVICE_MIN_RECORDED_S: i64 = 55 * 60;
/// Two snapshots further apart than this have a hole in the recording between them.
const RECORDING_GAP_S: i64 = 120;

/// Seconds of feed actually recorded from `from` on: the time between
/// consecutive snapshots, leaving out holes.
pub fn recorded_secs(snapshots: &[i64], from: i64) -> i64 {
    snapshots.windows(2).filter(|w| w[0] >= from && w[1] - w[0] <= RECORDING_GAP_S).map(|w| w[1] - w[0]).sum()
}

/// Historical times corrected by the last hour of bus movements, and the
/// trip times that follow from them.
pub struct LiveView {
    pub times: Blended,
    pub tt: Timetable,
    pub ratios: Arc<LiveRatios>,
    /// What each route was really seen to run in the last hour; `None`
    /// until nearly a full hour of the feed has been recorded.
    pub service: Option<Service>,
    /// Buses on their way along a route (not those standing at a first stop).
    pub buses_tracked: usize,
}

/// Everything needed to answer queries. Built once at start-up.
pub struct Engine {
    pub net: Network,
    /// Historical travel times: the model, or the timetable if there is too little data.
    pub times: Arc<dyn SegTimes>,
    /// Trip times under `times`, one timetable per kind of day.
    pub tt: Vec<Timetable>,
    /// Free text describing what `times` was fitted on, for /health.
    pub basis: String,
    /// Refreshed in the background while the server runs.
    pub live: RwLock<Option<Arc<LiveView>>>,
}

impl Engine {
    pub fn new(net: Network, times: Arc<dyn SegTimes>, basis: String) -> Engine {
        let tt = (0..crate::clock::DAY_KINDS).into_par_iter().map(|k| Timetable::build(&net, times.as_ref(), k)).collect();
        Engine { net, times, tt, basis, live: RwLock::new(None) }
    }

    /// Rebuild the live view from the newest archived snapshots. Returns a
    /// one-line description of what it found.
    pub fn refresh_live(&self, raw_dir: &Path) -> anyhow::Result<String> {
        let now = now_epoch();
        let from = now - WINDOW_S - 900;
        let mut files = Vec::new();
        for day in [Local::from_epoch(from).date_string(), Local::from_epoch(now).date_string()] {
            for f in rt::list_day(raw_dir, &day)? {
                if f.0 >= from && !files.contains(&f) {
                    files.push(f);
                }
            }
        }
        if files.last().is_none_or(|f| now - f.0 > LIVE_MAX_AGE_S) {
            *self.live.write().unwrap() = None;
            anyhow::bail!("no feed snapshot in the last {LIVE_MAX_AGE_S} s; live data starts once the feed is being recorded (serve --collect)");
        }
        let (trav, buses) = link::scan(&self.net, &files, now)?;
        let times_of: Vec<i64> = files.iter().map(|f| f.0).collect();
        let recorded = recorded_secs(&times_of, from);
        // Tracked buses are usable at once; the speed correction needs a longer recording.
        let warm = recorded >= LIVE_MIN_RECORDED_S;
        let ratios = Arc::new(LiveRatios::compute(&self.net, self.times.as_ref(), if warm { &trav } else { &[] }, now));
        let times = Blended { base: self.times.clone(), live: ratios.clone() };
        let tt = Timetable::build_live(&self.net, &times, Local::from_epoch(now).day_type(), now, &buses);
        let msg = format!(
            "live: {} traversals in the last hour on {} segments, city running at {:.2}x the usual time, {} buses tracked on {} routes",
            ratios.n_obs,
            ratios.n_segs,
            ratios.city_ratio(),
            buses.iter().filter(|b| b.waiting_s.is_none()).count(),
            buses.iter().filter(|b| b.waiting_s.is_none()).map(|b| b.route).collect::<HashSet<_>>().len()
        ) + &format!(", {} standing at a first stop", buses.iter().filter(|b| b.waiting_s.is_some_and(|w| w <= link::WAITING_MAX_S)).count())
            + &if warm { String::new() } else { format!("; speed correction starts once {} min of feed are recorded ({} so far)", LIVE_MIN_RECORDED_S / 60, recorded / 60) };
        let service = (recorded_secs(&times_of, now - WINDOW_S) >= SERVICE_MIN_RECORDED_S).then(|| Service::build(&self.net, &trav, &buses, now));
        let buses_tracked = buses.iter().filter(|b| b.waiting_s.is_none()).count();
        *self.live.write().unwrap() = Some(Arc::new(LiveView { times, tt, ratios, service, buses_tracked }));
        Ok(msg)
    }

    /// The live view, if it is fresh and the departure is close enough to now for it to matter.
    pub fn live_for(&self, depart: Local) -> Option<Arc<LiveView>> {
        let l = self.live.read().unwrap().clone()?;
        let lead = depart.epoch() - l.ratios.now;
        (now_epoch() - l.ratios.now <= LIVE_MAX_AGE_S && (-900..=LIVE_HORIZON_S).contains(&lead)).then_some(l)
    }
}

#[derive(Serialize, Clone)]
pub struct Point {
    pub name: String,
    pub lat: f64,
    pub lon: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_id: Option<String>,
}

#[derive(Serialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum LegOut {
    Walk {
        from: Point,
        to: Point,
        distance_m: u32,
        duration_s: u32,
        start: String,
        end: String,
    },
    Bus {
        route: String,
        route_id: String,
        headsign: String,
        agency: String,
        from: Point,
        to: Point,
        /// Stops ridden past, including the one you get off at.
        stops: u32,
        distance_m: u32,
        /// Time spent at the stop before the bus comes.
        wait_s: u32,
        start: String,
        end: String,
        duration_s: u32,
        /// What the published timetable says for the same ride.
        timetable_s: u32,
        /// The same ride at the quietest time of day.
        offpeak_s: u32,
        /// Share of the ride's segments estimated from each rung of the
        /// fallback ladder, e.g. {"1": 0.8, "5": 0.2}. 1 = this segment at
        /// this time of day; 7 = timetable.
        basis: std::collections::BTreeMap<String, f32>,
        /// How much of this ride's estimate rests on buses seen in the last hour (0 to 1).
        live: f32,
        /// Timetabled gap between buses on this route around boarding time.
        /// Buses keep roughly to this frequency but not to exact times, so for
        /// an untracked bus treat the boarding time as approximate.
        headway_s: u32,
        /// Where the boarding time comes from. "tracked": a real bus on its
        /// way, run forward from its GPS position. "at_first_stop": a real
        /// bus standing at the route's first stop, with an estimated
        /// departure. "timetable": a timetabled start.
        boarding: &'static str,
        /// Number plate when this is a real bus ("tracked" or "at_first_stop");
        /// absent when the boarding time comes from the timetable.
        #[serde(skip_serializing_if = "Option::is_none")]
        vehicle: Option<String>,
        /// What this route was really seen to run, when live data is in use
        /// and the boarding is not a tracked bus.
        #[serde(skip_serializing_if = "Option::is_none")]
        service: Option<ServiceOut>,
        /// [lat, lon] pairs along the road.
        geometry: Vec<[f64; 2]>,
    },
}

/// Observed service on a route, to set beside a boarding time that is not a tracked bus.
#[derive(Serialize)]
pub struct ServiceOut {
    /// Buses of this route seen passing the boarding stop in the last hour.
    pub seen_last_hour: u32,
    /// How many the timetable has in the same hour.
    pub timetabled_last_hour: u32,
    /// Buses of this route standing at its first stop right now.
    pub at_first_stop: u32,
}

#[derive(Serialize)]
pub struct OptionOut {
    /// When to leave the origin to catch the first bus without waiting.
    pub leave_at: String,
    pub arrive_at: String,
    /// Door to door, from `leave_at`.
    pub duration_s: u32,
    pub walk_s: u32,
    pub wait_s: u32,
    pub ride_s: u32,
    pub transfers: u32,
    /// Time between the requested departure and `leave_at`.
    pub slack_before_leaving_s: u32,
    /// The same journey with every ride at its quiet-hour time.
    pub offpeak_duration_s: u32,
    /// duration_s - offpeak_duration_s: what traffic adds right now.
    pub traffic_delay_s: i32,
    pub summary: String,
    pub legs: Vec<LegOut>,
}

#[derive(Serialize)]
pub struct Response {
    pub from: Point,
    pub to: Point,
    pub depart: String,
    pub day_type: &'static str,
    /// "model", "timetable", "model+live" or "timetable+live".
    pub travel_times: &'static str,
    /// Present when live data was used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live: Option<LiveInfo>,
    pub options: Vec<OptionOut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Serialize)]
pub struct LiveInfo {
    pub as_of: String,
    /// Bus segment traversals seen in the last hour.
    pub traversals: usize,
    /// City-wide: 1.2 means buses are taking 20% longer than usual right now.
    pub city_ratio: f32,
    /// Buses in service whose positions are being followed.
    pub buses_tracked: usize,
}

#[derive(Debug)]
pub enum PlanError {
    UnknownPlace(String),
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanError::UnknownPlace(p) => write!(f, "could not find a stop matching \"{p}\"; pass a stop name or \"lat,lon\""),
        }
    }
}

struct Resolved {
    point: Point,
    xy: P,
}

fn resolve(net: &Network, s: &str) -> Result<Resolved, PlanError> {
    if let Some((a, b)) = s.split_once(',') {
        if let (Ok(lat), Ok(lon)) = (a.trim().parse::<f64>(), b.trim().parse::<f64>()) {
            if (27.0..30.5).contains(&lat) && (75.5..78.5).contains(&lon) {
                return Ok(Resolved { point: Point { name: format!("{lat:.5},{lon:.5}"), lat, lon, stop_id: None }, xy: to_xy(lat, lon) });
            }
            return Err(PlanError::UnknownPlace(format!("{s} (outside the Delhi region)")));
        }
    }
    let place = net.find_place(s).ok_or_else(|| PlanError::UnknownPlace(s.to_string()))?;
    let [lat, lon] = round6(place.xy);
    Ok(Resolved { point: Point { name: place.label, lat, lon, stop_id: None }, xy: place.xy })
}

fn access_stops(net: &Network, xy: P) -> Vec<(u32, u32)> {
    let mut near = net.stops_near(xy, ACCESS_RADIUS_M);
    if near.len() < 3 {
        near = net.stops_near(xy, ACCESS_FALLBACK_M);
        near.truncate(5);
    }
    near.into_iter().map(|(s, d)| (s, walk_secs(d))).collect()
}

fn stop_point(net: &Network, s: u32) -> Point {
    let st = &net.stops[s as usize];
    let [lat, lon] = round6(st.xy);
    Point { name: st.name.clone(), lat, lon, stop_id: Some(st.id.clone()) }
}

fn round6(p: P) -> [f64; 2] {
    let (lat, lon) = to_ll(p);
    [(lat * 1e6).round() / 1e6, (lon * 1e6).round() / 1e6]
}

pub fn plan(eng: &Engine, from: &str, to: &str, depart: Local, max_options: usize) -> Result<Response, PlanError> {
    let net = &eng.net;
    let (o, d) = (resolve(net, from)?, resolve(net, to)?);
    let day_type = depart.day_type();
    let live = eng.live_for(depart);
    let (times, tt): (&dyn SegTimes, &Timetable) = match &live {
        Some(l) => (&l.times, &l.tt),
        None => (eng.times.as_ref(), &eng.tt[day_type]),
    };
    let sources = access_stops(net, o.xy);
    let targets = access_stops(net, d.xy);

    // Search, then search again with the routes already offered banned, so
    // the alternatives are genuinely different buses.
    let mut found: Vec<Journey> = Vec::new();
    let mut banned: HashSet<u32> = HashSet::new();
    for _ in 0..4 {
        let js = Search { net, tt, sources: &sources, targets: &targets, depart: depart.tod, max_rides: MAX_RIDES, banned: &banned }.run();
        let mut added = false;
        for j in js {
            if !found.iter().any(|f| f.routes() == j.routes()) {
                if let Some(Leg::Ride { route, .. }) = j.legs.iter().find(|l| matches!(l, Leg::Ride { .. })) {
                    // The route and its short-turn trips are one bus line to a rider.
                    banned.extend(net.family(*route));
                }
                found.push(j);
                added = true;
            }
        }
        if !added {
            break;
        }
    }
    // Rank by arrival time plus a penalty per change of bus: a direct bus that
    // arrives a few minutes later is the better answer for most riders.
    let cost = |j: &Journey| j.arrive + TRANSFER_PENALTY_S * (j.rides() as u32 - 1);
    found.sort_by_key(|j| (cost(j), j.rides()));
    // Drop options that are much worse than the best one.
    if let Some(best) = found.first().map(|j| cost(j) - depart.tod) {
        let limit = depart.tod + best + (best / 2).max(20 * 60);
        found.retain(|j| cost(j) <= limit);
    }
    found.truncate(max_options);

    let mut options: Vec<OptionOut> = found.iter().map(|j| render(net, times, tt, live.as_deref(), &o, &d, depart, day_type, j)).collect();

    // Walking the whole way, when it is short enough to be a real option.
    let direct = dist(o.xy, d.xy) * crate::gtfs::WALK_DETOUR;
    if direct <= WALK_ONLY_M {
        let secs = walk_secs(dist(o.xy, d.xy));
        options.push(OptionOut {
            leave_at: depart.iso(),
            arrive_at: depart.plus(secs as i64).iso(),
            duration_s: secs,
            walk_s: secs,
            wait_s: 0,
            ride_s: 0,
            transfers: 0,
            slack_before_leaving_s: 0,
            offpeak_duration_s: secs,
            traffic_delay_s: 0,
            summary: format!("Walk {} m", direct.round()),
            legs: vec![LegOut::Walk {
                from: o.point.clone(),
                to: d.point.clone(),
                distance_m: direct.round() as u32,
                duration_s: secs,
                start: depart.iso(),
                end: depart.plus(secs as i64).iso(),
            }],
        });
        options.sort_by_key(|x| x.arrive_at.clone());
    }

    let note = if options.is_empty() {
        Some(format!(
            "No bus journey found within {MAX_RIDES} buses. {} stops near the origin, {} near the destination.",
            sources.len(),
            targets.len()
        ))
    } else {
        None
    };
    Ok(Response {
        from: o.point,
        to: d.point,
        depart: depart.iso(),
        day_type: crate::clock::DAY_KIND_NAMES[day_type],
        travel_times: times.name(),
        live: live.as_ref().map(|l| LiveInfo {
            as_of: Local::from_epoch(l.ratios.now).iso(),
            traversals: l.ratios.n_obs,
            city_ratio: (l.ratios.city_ratio() * 100.0).round() / 100.0,
            buses_tracked: l.buses_tracked,
        }),
        options,
        note,
    })
}

#[allow(clippy::too_many_arguments)]
fn render(net: &Network, times: &dyn SegTimes, tt: &Timetable, live: Option<&LiveView>, o: &Resolved, d: &Resolved, depart: Local, day_type: usize, j: &Journey) -> OptionOut {
    // Seconds-since-midnight on the departure date -> ISO string.
    let iso = |t: u32| Local { day: depart.day, tod: 0 }.plus(t as i64).iso();
    let first_board = j.legs.iter().find_map(|l| if let Leg::Ride { board_time, .. } = l { Some(*board_time) } else { None }).unwrap_or(depart.tod);
    let leave = first_board.saturating_sub(j.access.1).max(depart.tod);

    let mut legs: Vec<LegOut> = Vec::new();
    let (mut walk_s, mut wait_s, mut ride_s, mut offpeak_ride, mut rides) = (j.access.1 + j.egress.1, 0u32, 0u32, 0f32, 0u32);
    let mut names: Vec<String> = Vec::new();

    let s0 = &net.stops[j.access.0 as usize];
    legs.push(LegOut::Walk {
        from: o.point.clone(),
        to: stop_point(net, j.access.0),
        distance_m: (dist(o.xy, s0.xy) * crate::gtfs::WALK_DETOUR).round() as u32,
        duration_s: j.access.1,
        start: iso(leave),
        end: iso(leave + j.access.1),
    });
    // When the traveller is standing at the next boarding stop.
    let mut ready = leave + j.access.1;

    for leg in &j.legs {
        match *leg {
            Leg::Transfer { from, to, secs } => {
                let (a, b) = (&net.stops[from as usize], &net.stops[to as usize]);
                push_walk(
                    &mut legs,
                    stop_point(net, from),
                    stop_point(net, to),
                    (dist(a.xy, b.xy) * crate::gtfs::WALK_DETOUR).round() as u32,
                    secs,
                    iso(ready),
                    iso(ready + secs),
                );
                walk_s += secs;
                ready += secs;
            }
            Leg::Ride { route, board, alight, board_time, alight_time, .. } => {
                let r = &net.routes[route as usize];
                let (b, a) = (board as usize, alight as usize);
                let wait = board_time.saturating_sub(ready);
                let dur = alight_time - board_time;
                let mut offpeak = 0f32;
                let mut timetable = 0f32;
                let mut metres = 0f32;
                let mut basis: std::collections::BTreeMap<u8, u32> = Default::default();
                let mut live_w = 0f32;
                let n = r.stops.len();
                for k in b..a {
                    let seg = r.segs[k];
                    offpeak += times.quiet_secs(net, seg);
                    timetable += net.segments[seg as usize].sched_s;
                    metres += net.segments[seg as usize].len_m;
                    // Time the bus enters this segment, from the trip's own stop times.
                    let enter = tt.at(route, trip_of(leg), k, n) % 86_400;
                    *basis.entry(times.level(seg, day_type, enter)).or_default() += 1;
                    live_w += times.live_weight(seg, enter);
                }
                let total = (a - b) as f32;
                let mut geometry = vec![round6(net.stops[r.stops[b] as usize].xy)];
                geometry.extend(r.shape.slice(r.along[b], r.along[a]).into_iter().map(round6));
                geometry.push(round6(net.stops[r.stops[a] as usize].xy));
                names.push(r.name.clone());
                let vehicle = tt.vehicle(route, trip_of(leg)).map(String::from);
                let boarding = match &vehicle {
                    Some(_) if tt.is_standing(route, trip_of(leg)) => "at_first_stop",
                    Some(_) => "tracked",
                    None => "timetable",
                };
                // Buses are counted on the route's own entry, whichever trip is boarded.
                let (seen_route, seen_pos) = net.on_main(route, b).unwrap_or((route, b));
                let service = live.filter(|_| boarding != "tracked").and_then(|l| {
                    l.service.as_ref().map(|s| ServiceOut {
                        seen_last_hour: s.seen(seen_route, seen_pos),
                        timetabled_last_hour: Service::timetabled(net, route, b, Local::from_epoch(l.ratios.now).tod),
                        at_first_stop: s.standing(seen_route),
                    })
                });
                legs.push(LegOut::Bus {
                    route: r.name.clone(),
                    route_id: r.id.clone(),
                    headsign: r.desc.clone(),
                    agency: r.agency.clone(),
                    from: stop_point(net, r.stops[b]),
                    to: stop_point(net, r.stops[a]),
                    stops: (a - b) as u32,
                    distance_m: metres.round() as u32,
                    wait_s: wait,
                    start: iso(board_time),
                    end: iso(alight_time),
                    duration_s: dur,
                    timetable_s: timetable.round() as u32,
                    offpeak_s: offpeak.round() as u32,
                    basis: basis.into_iter().map(|(k, v)| (k.to_string(), (v as f32 / total * 100.0).round() / 100.0)).collect(),
                    live: (live_w / total * 100.0).round() / 100.0,
                    headway_s: headway(&net.starts_through(route, b), board_time.saturating_sub(r.sched[b])),
                    boarding,
                    vehicle,
                    service,
                    geometry,
                });
                wait_s += wait;
                ride_s += dur;
                offpeak_ride += offpeak;
                rides += 1;
                ready = alight_time;
            }
        }
    }

    let s1 = &net.stops[j.egress.0 as usize];
    push_walk(
        &mut legs,
        stop_point(net, j.egress.0),
        d.point.clone(),
        (dist(s1.xy, d.xy) * crate::gtfs::WALK_DETOUR).round() as u32,
        j.egress.1,
        iso(ready),
        iso(ready + j.egress.1),
    );
    let arrive = ready + j.egress.1;
    let duration = arrive - leave;
    let offpeak_duration = duration - ride_s + offpeak_ride.round() as u32;
    OptionOut {
        leave_at: iso(leave),
        arrive_at: iso(arrive),
        duration_s: duration,
        walk_s,
        wait_s,
        ride_s,
        transfers: rides.saturating_sub(1),
        slack_before_leaving_s: leave - depart.tod,
        offpeak_duration_s: offpeak_duration,
        traffic_delay_s: duration as i32 - offpeak_duration as i32,
        summary: names.join(" → "),
        legs,
    }
}

/// Append a walk, folding it into the previous leg if that was also a walk
/// (a transfer walk followed by the final walk reads better as one).
fn push_walk(legs: &mut Vec<LegOut>, from: Point, to: Point, distance_m: u32, duration_s: u32, start: String, end: String) {
    if let Some(LegOut::Walk { to: prev_to, distance_m: d, duration_s: t, end: e, .. }) = legs.last_mut() {
        *prev_to = to;
        *d += distance_m;
        *t += duration_s;
        *e = end;
        return;
    }
    legs.push(LegOut::Walk { from, to, distance_m, duration_s, start, end });
}

/// Gap between the timetabled departures either side of `start`, seconds.
fn headway(starts: &[u32], start: u32) -> u32 {
    let i = starts.partition_point(|&s| s < start);
    let gap = |a: usize, b: usize| starts.get(b).zip(starts.get(a)).map(|(y, x)| y - x);
    let around = if i == 0 { gap(0, 1) } else { gap(i - 1, i).or_else(|| gap(i.saturating_sub(2), i - 1)) };
    around.unwrap_or(0)
}

fn trip_of(leg: &Leg) -> u32 {
    match leg {
        Leg::Ride { trip, .. } => *trip,
        _ => 0,
    }
}

/// One line per option, for the command line.
pub fn describe(r: &Response) -> String {
    let hm = |iso: &str| iso.get(11..16).unwrap_or("").to_string();
    let mut s = format!("{} → {}  depart {} ({}, {} times)\n", r.from.name, r.to.name, r.depart, r.day_type, r.travel_times);
    if let Some(l) = &r.live {
        s += &format!("live as of {}: {} traversals in the last hour, city at {:.2}x usual, {} buses tracked\n", l.as_of, l.traversals, l.city_ratio, l.buses_tracked);
    }
    for (i, o) in r.options.iter().enumerate() {
        s += &format!(
            "\n{}. {}  leave {} arrive {}  {} min  (walk {}, wait {}, ride {} min; off-peak {} min, traffic {:+} min)\n",
            i + 1,
            if o.summary.is_empty() { "walk" } else { &o.summary },
            hm(&o.leave_at),
            hm(&o.arrive_at),
            o.duration_s / 60,
            o.walk_s / 60,
            o.wait_s / 60,
            o.ride_s / 60,
            o.offpeak_duration_s / 60,
            o.traffic_delay_s / 60
        );
        for l in &o.legs {
            match l {
                LegOut::Walk { from, to, distance_m, duration_s, .. } => {
                    s += &format!("     walk {} m ({} min): {} → {}\n", distance_m, duration_s / 60, from.name, to.name)
                }
                LegOut::Bus { route, from, to, stops, wait_s, start, end, duration_s, timetable_s, basis, live, vehicle, boarding, .. } => {
                    s += &format!(
                        "     bus {route} [{}] {}–{} ({} min, timetable {} min, wait {} min, {stops} stops): {} → {}  basis {:?} live {live}\n",
                        vehicle.as_deref().map_or("scheduled".to_string(), |v| format!("{} {v}", if *boarding == "tracked" { "tracked" } else { "standing at first stop" })),
                        hm(start),
                        hm(end),
                        duration_s / 60,
                        timetable_s / 60,
                        wait_s / 60,
                        from.name,
                        to.name,
                        basis
                    )
                }
            }
        }
    }
    if let Some(n) = &r.note {
        s += &format!("\n{n}\n");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recorded_time_leaves_out_holes_and_what_came_before_the_window() {
        // Snapshots every 30 s for 5 minutes, a 10-minute hole, then 2 more minutes.
        let mut t: Vec<i64> = (0..=10).map(|i| i * 30).collect();
        t.extend((0..=4).map(|i| 900 + i * 30));
        assert_eq!(recorded_secs(&t, 0), 300 + 120);
        // Only the part from t = 150 on.
        assert_eq!(recorded_secs(&t, 150), 150 + 120);
        assert_eq!(recorded_secs(&[], 0), 0);
        assert_eq!(recorded_secs(&[40], 0), 0);
    }
}
