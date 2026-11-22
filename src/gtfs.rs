//! Load the static GTFS and build the network everything else works on.
//!
//! Facts about this feed that the loader relies on (measured):
//! a route_id is one direction, and trips with the same stop pattern have
//! identical stop offsets. In the feed of August 2026 every route has one
//! shape and one stop pattern. The feed of October 2026 (v32) adds short-turn
//! trips, which run only the first part of their route and carry a shape of
//! their own. So the loader groups a route's trips by stop pattern: the
//! longest pattern is the route itself, and every other pattern becomes a
//! variant entry that the journey search treats as a route of its own.

use crate::clock::parse_hms;
use crate::geom::{P, Polyline, dist, monotone_project, to_xy};
use anyhow::{Context, Result, anyhow};
use std::collections::HashMap;
use std::path::Path;

pub struct Stop {
    pub id: String,
    pub name: String,
    pub lat: f64,
    pub lon: f64,
    pub xy: P,
}

pub struct Route {
    pub id: String,
    /// e.g. "425UP".
    pub name: String,
    /// e.g. "Azadpur Terminal to Rohini Sec 22 Terminal".
    pub desc: String,
    pub agency: String,
    /// Stop indices in travel order.
    pub stops: Vec<u32>,
    /// Timetable offset of each stop from the first, seconds (synthetic: 18 km/h).
    pub sched: Vec<u32>,
    /// Scheduled departure of every trip from the first stop, sorted, seconds since midnight.
    pub starts: Vec<u32>,
    pub shape: Polyline,
    /// Distance of each stop along the shape, metres, non-decreasing.
    pub along: Vec<f64>,
    /// Segment index for each consecutive stop pair (len = stops.len() - 1).
    pub segs: Vec<u32>,
    /// Index of the entry that holds this GTFS route's full stop pattern: its
    /// own index, except for a short-turn variant. Buses are linked to that
    /// entry, because the feed does not say which trips turn short.
    pub main: u32,
}

/// A directed stop -> next-stop hop, shared by every route that makes it.
pub struct Segment {
    pub a: u32,
    pub b: u32,
    /// Along-shape length, metres (mean over routes using it).
    pub len_m: f32,
    /// Timetable time, seconds.
    pub sched_s: f32,
    pub n_routes: u32,
}

pub struct Network {
    pub stops: Vec<Stop>,
    pub routes: Vec<Route>,
    pub segments: Vec<Segment>,
    pub stop_index: HashMap<String, u32>,
    pub route_index: HashMap<String, u32>,
    pub seg_index: HashMap<(u32, u32), u32>,
    /// For each stop: (route, position in that route's stop list).
    pub stop_routes: Vec<Vec<(u32, u16)>>,
    /// For each stop: nearby stops reachable on foot and the walk in seconds.
    pub footpaths: Vec<Vec<(u32, u32)>>,
    /// Segments that directly precede or follow each segment on some route.
    pub seg_neighbours: Vec<Vec<u32>>,
    /// For each route, the opposite-direction route ("425UP" <-> "425DOWN"), if any.
    pub siblings: Vec<Option<u32>>,
    /// For each route, the entries that hold its short-turn trips (usually none).
    /// GTFS routes come first in `routes`, in file order; variants follow them.
    pub variants: Vec<Vec<u32>>,
}

/// Straight-line metres are multiplied by this to approximate street walking.
pub const WALK_DETOUR: f64 = 1.3;
/// Walking speed, m/s (4.5 km/h).
pub const WALK_SPEED: f64 = 1.25;
/// Stops closer than this are linked for transfers on foot.
pub const TRANSFER_RADIUS_M: f64 = 300.0;

pub fn walk_secs(metres: f64) -> u32 {
    (metres * WALK_DETOUR / WALK_SPEED).round() as u32
}

fn reader(dir: &Path, name: &str) -> Result<csv::Reader<std::fs::File>> {
    let p = dir.join(name);
    csv::ReaderBuilder::new().flexible(true).from_path(&p).with_context(|| format!("opening {}", p.display()))
}

fn col(h: &csv::StringRecord, name: &str) -> Result<usize> {
    h.iter().position(|c| c.trim_start_matches('\u{feff}') == name).ok_or_else(|| anyhow!("column {name} missing"))
}

impl Network {
    pub fn load(dir: &Path) -> Result<Network> {
        // ---- stops
        let mut rd = reader(dir, "stops.txt")?;
        let h = rd.headers()?.clone();
        let (ci, cn, cla, clo) = (col(&h, "stop_id")?, col(&h, "stop_name")?, col(&h, "stop_lat")?, col(&h, "stop_lon")?);
        let mut stops = Vec::new();
        let mut stop_index = HashMap::new();
        for r in rd.records() {
            let r = r?;
            let (lat, lon): (f64, f64) = (r[cla].trim().parse()?, r[clo].trim().parse()?);
            stop_index.insert(r[ci].to_string(), stops.len() as u32);
            stops.push(Stop { id: r[ci].to_string(), name: r[cn].trim().to_string(), lat, lon, xy: to_xy(lat, lon) });
        }

        // ---- routes
        let mut rd = reader(dir, "routes.txt")?;
        let h = rd.headers()?.clone();
        let (ci, cs, cd, ca) = (col(&h, "route_id")?, col(&h, "route_short_name")?, col(&h, "route_desc")?, col(&h, "agency_id")?);
        let mut meta: Vec<(String, String, String, String)> = Vec::new();
        let mut route_index = HashMap::new();
        for r in rd.records() {
            let r = r?;
            route_index.insert(r[ci].to_string(), meta.len() as u32);
            meta.push((r[ci].to_string(), r[cs].to_string(), r[cd].to_string(), r[ca].to_string()));
        }

        // ---- trips: route and shape of each trip
        let mut rd = reader(dir, "trips.txt")?;
        let h = rd.headers()?.clone();
        let (cr, ct, csh) = (col(&h, "route_id")?, col(&h, "trip_id")?, col(&h, "shape_id")?);
        let mut trip_route: HashMap<String, u32> = HashMap::new();
        let mut trip_shape: HashMap<String, String> = HashMap::new();
        for r in rd.records() {
            let r = r?;
            let ri = *route_index.get(&r[cr]).ok_or_else(|| anyhow!("trip {} on unknown route {}", &r[ct], &r[cr]))?;
            trip_route.insert(r[ct].to_string(), ri);
            trip_shape.insert(r[ct].to_string(), r[csh].to_string());
        }

        // ---- stop_times: (seq, stop, arrival) per trip
        let mut rd = reader(dir, "stop_times.txt")?;
        let h = rd.headers()?.clone();
        let (ct, ca, cs, cq) = (col(&h, "trip_id")?, col(&h, "arrival_time")?, col(&h, "stop_id")?, col(&h, "stop_sequence")?);
        let mut per_route: Vec<HashMap<String, Vec<(u32, u32, u32)>>> = (0..meta.len()).map(|_| HashMap::new()).collect();
        let mut rec = csv::StringRecord::new();
        while rd.read_record(&mut rec)? {
            let Some(&ri) = trip_route.get(&rec[ct]) else { continue };
            let si = *stop_index.get(&rec[cs]).ok_or_else(|| anyhow!("stop_time refers to unknown stop {}", &rec[cs]))?;
            let t = parse_hms(&rec[ca]).ok_or_else(|| anyhow!("bad time {}", &rec[ca]))?;
            let seq: u32 = rec[cq].trim().parse()?;
            per_route[ri as usize].entry(rec[ct].to_string()).or_default().push((seq, si, t));
        }

        // ---- shapes
        let mut rd = reader(dir, "shapes.txt")?;
        let h = rd.headers()?.clone();
        let (ci, cla, clo, cq) =
            (col(&h, "shape_id")?, col(&h, "shape_pt_lat")?, col(&h, "shape_pt_lon")?, col(&h, "shape_pt_sequence")?);
        let mut shape_pts: HashMap<String, Vec<(u32, P)>> = HashMap::new();
        let mut rec = csv::StringRecord::new();
        while rd.read_record(&mut rec)? {
            let p = to_xy(rec[cla].trim().parse()?, rec[clo].trim().parse()?);
            shape_pts.entry(rec[ci].to_string()).or_default().push((rec[cq].trim().parse()?, p));
        }

        // ---- assemble routes: one entry per route_id for its longest stop
        // pattern, then one entry per other pattern (short-turn trips).
        let polyline = |shape_id: &str| -> Option<Polyline> {
            let mut pts = shape_pts.get(shape_id)?.clone();
            pts.sort_by_key(|p| p.0);
            Some(Polyline::new(pts.into_iter().map(|p| p.1).collect()))
        };
        let mut routes = Vec::with_capacity(meta.len());
        let mut pending: Vec<(usize, Route)> = Vec::new();
        let mut dropped = 0;
        let mut irregular = 0;
        let mut old_to_new: Vec<Option<u32>> = vec![None; meta.len()];
        for (ri, (id, name, desc, agency)) in meta.into_iter().enumerate() {
            // Trips grouped by stop pattern, each with its trip id (for its shape).
            let mut groups: HashMap<Vec<u32>, Vec<(String, Vec<(u32, u32, u32)>)>> = HashMap::new();
            for (trip_id, mut t) in std::mem::take(&mut per_route[ri]) {
                t.sort_unstable();
                if t.len() >= 2 {
                    groups.entry(t.iter().map(|x| x.1).collect()).or_default().push((trip_id, t));
                }
            }
            // Longest pattern first; then the one with more trips; then by its stops.
            let mut groups: Vec<_> = groups.into_iter().collect();
            groups.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then(b.1.len().cmp(&a.1.len())).then(a.0.cmp(&b.0)));
            if groups.is_empty() {
                dropped += 1;
                continue;
            }
            let mut main_shape_id: Option<&String> = None;
            for (gi, (stop_seq, trips)) in groups.into_iter().enumerate() {
                // The earliest trip gives the offsets and the shape.
                let Some((first_id, first)) = trips.iter().min_by(|a, b| a.1[0].2.cmp(&b.1[0].2).then_with(|| a.0.cmp(&b.0))) else { continue };
                let sched: Vec<u32> = first.iter().map(|x| x.2 - first[0].2).collect();
                // Verify the "identical offsets" assumption.
                for (_, t) in &trips {
                    if !t.iter().zip(&sched).all(|(a, off)| (a.2 - t[0].2).abs_diff(*off) <= 2) {
                        irregular += 1;
                    }
                }
                let mut starts: Vec<u32> = trips.iter().map(|(_, t)| t[0].2).collect();
                starts.sort_unstable();
                let shape_id = trip_shape.get(first_id);
                let own = shape_id.and_then(|s| polyline(s));
                let shape = if gi == 0 {
                    let Some(shape) = own else {
                        dropped += 1;
                        break;
                    };
                    main_shape_id = shape_id;
                    shape
                } else {
                    // A variant without a shape of its own runs along the route's.
                    match own.or_else(|| main_shape_id.and_then(|s| polyline(s))) {
                        Some(shape) => shape,
                        None => continue,
                    }
                };
                let stop_xy: Vec<P> = stop_seq.iter().map(|&s| stops[s as usize].xy).collect();
                let along = monotone_project(&shape, &stop_xy);
                let route = Route {
                    id: id.clone(),
                    name: name.clone(),
                    desc: desc.clone(),
                    agency: agency.clone(),
                    stops: stop_seq,
                    sched,
                    starts,
                    shape,
                    along,
                    segs: vec![],
                    main: routes.len() as u32,
                };
                if gi == 0 {
                    old_to_new[ri] = Some(routes.len() as u32);
                    routes.push(route);
                } else {
                    pending.push((ri, route));
                }
            }
        }
        let mut variants: Vec<Vec<u32>> = vec![Vec::new(); routes.len()];
        for (ri, mut route) in pending {
            let Some(main) = old_to_new[ri] else { continue };
            route.main = main;
            variants[main as usize].push(routes.len() as u32);
            routes.push(route);
        }
        variants.resize(routes.len(), Vec::new());
        if dropped > 0 {
            eprintln!("warning: {dropped} routes dropped (no usable trips or shape)");
        }
        if irregular > 0 {
            eprintln!("warning: {irregular} trips do not keep their stop pattern's timetable offsets; the offsets of the pattern's first trip are used");
        }
        let route_index: HashMap<String, u32> =
            route_index.into_iter().filter_map(|(k, v)| old_to_new[v as usize].map(|n| (k, n))).collect();

        // ---- segments
        let mut seg_index: HashMap<(u32, u32), u32> = HashMap::new();
        let mut acc: Vec<(u32, u32, f64, f64, u32)> = Vec::new(); // a, b, sum_len, sched, n
        for (ri, r) in routes.iter_mut().enumerate() {
            let variant = r.main as usize != ri;
            for k in 0..r.stops.len() - 1 {
                let (a, b) = (r.stops[k], r.stops[k + 1]);
                let straight = dist(stops[a as usize].xy, stops[b as usize].xy);
                // Along-shape length, never shorter than the straight line.
                let len = (r.along[k + 1] - r.along[k]).max(straight).max(1.0);
                let sched = (r.sched[k + 1] - r.sched[k]) as f64;
                let id = *seg_index.entry((a, b)).or_insert_with(|| {
                    acc.push((a, b, 0.0, sched, 0));
                    (acc.len() - 1) as u32
                });
                // A short-turn variant repeats its route's segments: count each route once.
                if !variant || acc[id as usize].4 == 0 {
                    acc[id as usize].2 += len;
                    acc[id as usize].4 += 1;
                }
                r.segs.push(id);
            }
        }
        let segments: Vec<Segment> = acc
            .iter()
            .map(|&(a, b, sum, sched, n)| {
                let len_m = (sum / n as f64) as f32;
                // 64 hops have zero scheduled time (co-located stops); give them 18 km/h too.
                let sched_s = if sched > 0.0 { sched as f32 } else { (len_m / 5.0).max(1.0) };
                Segment { a, b, len_m, sched_s, n_routes: n }
            })
            .collect();

        let mut seg_neighbours: Vec<Vec<u32>> = vec![Vec::new(); segments.len()];
        for r in &routes {
            for w in r.segs.windows(2) {
                seg_neighbours[w[0] as usize].push(w[1]);
                seg_neighbours[w[1] as usize].push(w[0]);
            }
        }
        for v in seg_neighbours.iter_mut() {
            v.sort_unstable();
            v.dedup();
        }

        // ---- which routes serve each stop
        let mut stop_routes: Vec<Vec<(u32, u16)>> = vec![Vec::new(); stops.len()];
        for (ri, r) in routes.iter().enumerate() {
            for (pos, &s) in r.stops.iter().enumerate() {
                stop_routes[s as usize].push((ri as u32, pos as u16));
            }
        }

        // ---- walking transfers, via a coarse grid
        let cell = TRANSFER_RADIUS_M;
        let key = |p: P| ((p.0 / cell).floor() as i64, (p.1 / cell).floor() as i64);
        let mut grid: HashMap<(i64, i64), Vec<u32>> = HashMap::new();
        for (i, s) in stops.iter().enumerate() {
            grid.entry(key(s.xy)).or_default().push(i as u32);
        }
        let mut footpaths: Vec<Vec<(u32, u32)>> = vec![Vec::new(); stops.len()];
        for (i, s) in stops.iter().enumerate() {
            let (gx, gy) = key(s.xy);
            for dx in -1..=1 {
                for dy in -1..=1 {
                    for &o in grid.get(&(gx + dx, gy + dy)).map(|v| v.as_slice()).unwrap_or(&[]) {
                        let d = dist(s.xy, stops[o as usize].xy);
                        if o as usize != i && d <= TRANSFER_RADIUS_M {
                            footpaths[i].push((o, walk_secs(d)));
                        }
                    }
                }
            }
        }

        let by_name: HashMap<&str, u32> =
            routes.iter().enumerate().filter(|(i, r)| r.main as usize == *i).map(|(i, r)| (r.name.as_str(), i as u32)).collect();
        let siblings: Vec<Option<u32>> = routes
            .iter()
            .enumerate()
            .map(|(i, r)| {
                if r.main as usize != i {
                    return None;
                }
                let other = if let Some(b) = r.name.strip_suffix("UP") {
                    format!("{b}DOWN")
                } else if let Some(b) = r.name.strip_suffix("DOWN") {
                    format!("{b}UP")
                } else {
                    return None;
                };
                by_name.get(other.as_str()).copied()
            })
            .collect();

        Ok(Network { stops, routes, segments, stop_index, route_index, seg_index, stop_routes, footpaths, seg_neighbours, siblings, variants })
    }

    /// True for a GTFS route's own entry, false for a short-turn variant of one.
    pub fn is_main(&self, route: u32) -> bool {
        self.routes[route as usize].main == route
    }

    /// The GTFS route's own entry followed by its short-turn variants.
    pub fn family(&self, route: u32) -> impl Iterator<Item = u32> + '_ {
        let main = self.routes[route as usize].main;
        std::iter::once(main).chain(self.variants.get(main as usize).into_iter().flatten().copied())
    }

    /// Stop position `pos` of `route` as a position on the GTFS route's own
    /// entry, which is where buses are linked. A short-turn variant shares its
    /// first stops with it, so the position is normally unchanged.
    pub fn on_main(&self, route: u32, pos: usize) -> Option<(u32, usize)> {
        let r = &self.routes[route as usize];
        if r.main == route {
            return Some((route, pos));
        }
        let (m, stop) = (&self.routes[r.main as usize], *r.stops.get(pos)?);
        if m.stops.get(pos) == Some(&stop) {
            return Some((r.main, pos));
        }
        m.stops.iter().position(|&s| s == stop).map(|p| (r.main, p))
    }

    /// Timetabled departures from the first stop of every trip of this GTFS
    /// route that calls at position `pos` of `route`, short-turn trips
    /// included, sorted. For a route without variants this is its `starts`.
    pub fn starts_through(&self, route: u32, pos: usize) -> Vec<u32> {
        let stop = self.routes[route as usize].stops.get(pos).copied();
        let mut v: Vec<u32> = self
            .family(route)
            .filter(|&e| e == route || (stop.is_some() && self.routes[e as usize].stops.get(pos).copied() == stop))
            .flat_map(|e| self.routes[e as usize].starts.iter().copied())
            .collect();
        v.sort_unstable();
        v
    }

    /// GTFS routes calling at a stop (a route that passes twice counts twice).
    pub fn routes_at(&self, stop: usize) -> usize {
        self.stop_routes[stop].iter().filter(|x| self.is_main(x.0)).count()
    }

    /// The opposite-direction route, if the feed has one.
    pub fn sibling(&self, route: u32) -> Option<u32> {
        self.siblings.get(route as usize).copied().flatten()
    }

    /// Stops within `radius` metres of a point, nearest first.
    pub fn stops_near(&self, p: P, radius: f64) -> Vec<(u32, f64)> {
        let mut v: Vec<(u32, f64)> = self
            .stops
            .iter()
            .enumerate()
            .filter_map(|(i, s)| {
                let d = dist(p, s.xy);
                (d <= radius && !self.stop_routes[i].is_empty()).then_some((i as u32, d))
            })
            .collect();
        v.sort_by(|a, b| a.1.total_cmp(&b.1));
        v
    }

    pub fn summary(&self) -> String {
        let trips: usize = self.routes.iter().map(|r| r.starts.len()).sum();
        let fp: usize = self.footpaths.iter().map(|f| f.len()).sum();
        let mains = (0..self.routes.len() as u32).filter(|&r| self.is_main(r)).count();
        let with_variants = self.variants.iter().filter(|v| !v.is_empty()).count();
        let short: usize = (0..self.routes.len() as u32).filter(|&r| !self.is_main(r)).map(|r| self.routes[r as usize].starts.len()).sum();
        let note = if with_variants > 0 { format!(" ({short} of them short-turn trips, on {with_variants} routes)") } else { String::new() };
        format!("{} stops, {} routes, {} trips{}, {} segments, {} walking links", self.stops.len(), mains, trips, note, self.segments.len(), fp)
    }
}

/// Lower-case alphanumeric tokens, for name matching.
pub fn tokens(s: &str) -> Vec<String> {
    s.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|t| !t.is_empty()).map(String::from).collect()
}

/// A named place resolved to a point and the stops that carry the name.
pub struct Place {
    pub label: String,
    pub xy: P,
}

impl Network {
    /// Resolve a stop name to a place. Stop names are not unique (one name is
    /// usually both sides of a road), so the result is the cluster of stops
    /// sharing the best-matching name and the point at their centre.
    pub fn find_place(&self, query: &str) -> Option<Place> {
        let q = tokens(query);
        if q.is_empty() {
            return None;
        }
        // Score every distinct stop name: all query tokens must appear (as a
        // token prefix); fewer extra tokens is better; more routes breaks ties.
        let mut best: Option<(usize, usize, usize, &str)> = None; // (missing, extra, -routes, name)
        for (i, s) in self.stops.iter().enumerate() {
            if self.stop_routes[i].is_empty() {
                continue;
            }
            let t = tokens(&s.name);
            let hit = q.iter().filter(|qt| t.iter().any(|x| x.starts_with(qt.as_str()))).count();
            if hit == 0 {
                continue;
            }
            let key = (q.len() - hit, t.len().saturating_sub(hit), usize::MAX - self.routes_at(i), s.name.as_str());
            if best.is_none_or(|b| key < b) {
                best = Some(key);
            }
        }
        let (missing, _, _, name) = best?;
        // Refuse a match that drops more than half of what was asked for.
        if missing * 2 > q.len() {
            return None;
        }
        let want = tokens(name);
        let members: Vec<u32> =
            (0..self.stops.len() as u32).filter(|&i| tokens(&self.stops[i as usize].name) == want).collect();
        let n = members.len() as f64;
        let (sx, sy) = members.iter().fold((0.0, 0.0), |a, &i| (a.0 + self.stops[i as usize].xy.0, a.1 + self.stops[i as usize].xy.1));
        Some(Place { label: name.to_string(), xy: (sx / n, sy / n) })
    }

    /// Up to `limit` distinct stop names matching a query, for autocomplete.
    pub fn search_names(&self, query: &str, limit: usize) -> Vec<(String, f64, f64, usize)> {
        let q = tokens(query);
        let mut seen: HashMap<String, (usize, usize, f64, f64, usize)> = HashMap::new();
        for (i, s) in self.stops.iter().enumerate() {
            let t = tokens(&s.name);
            if !q.is_empty() && q.iter().all(|qt| t.iter().any(|x| x.starts_with(qt.as_str()))) {
                let e = seen.entry(s.name.clone()).or_insert((t.len(), 0, s.lat, s.lon, 0));
                e.1 += self.routes_at(i);
                e.4 += 1;
            }
        }
        let mut v: Vec<_> = seen.into_iter().collect();
        v.sort_by(|a, b| (a.1.0, usize::MAX - a.1.1).cmp(&(b.1.0, usize::MAX - b.1.1)).then(a.0.cmp(&b.0)));
        v.into_iter().take(limit).map(|(n, e)| (n, e.2, e.3, e.1)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::Local;
    use crate::link::VehicleState;
    use crate::raptor::{Leg, Scheduled, Search, Timetable};
    use std::collections::HashSet;

    /// Five stops on a line. Route 10 ("9UP") has two full trips (08:00,
    /// 09:00) and two short-turn trips over its first three stops (06:00,
    /// 07:00), listed first and with a shape of their own, as in the feed of
    /// October 2026. Route 11 ("9DOWN") runs the other way.
    fn feed() -> Network {
        let dir = std::env::temp_dir().join(format!("busplan-gtfs-test-{}-{:?}", std::process::id(), std::thread::current().id()));
        std::fs::create_dir_all(&dir).unwrap();
        let lat = |i: usize| 28.6 + 0.005 * i as f64;
        let mut stops = String::from("stop_id,stop_code,stop_name,stop_lat,stop_lon\n");
        for i in 1..=5 {
            stops += &format!("s{i},C{i},Stop {i},{},77.2\n", lat(i));
        }
        let routes = "agency_id,route_id,route_long_name,route_short_name,route_desc,route_type\nDTC,10,9UP,9UP,One to Five,3\nDTC,11,9DOWN,9DOWN,Five to One,3\n";
        let mut trips = String::from("route_id,service_id,trip_id,trip_headsign,trip_short_name,direction_id,block_id,shape_id,wheelchair_accessible,bikes_allowed\n");
        let mut times = String::from("trip_id,arrival_time,departure_time,stop_id,stop_sequence,fare_stage,stop_dist\n");
        let mut trip = |route: &str, id: &str, shape: &str, hour: u32, seq: &[usize]| {
            trips += &format!("{route},1,{id},,,,,{shape},0,0\n");
            for (k, s) in seq.iter().enumerate() {
                let t = format!("{hour:02}:{:02}:00", 2 * k);
                times += &format!("{id},{t},{t},s{s},{k},0,\n");
            }
        };
        trip("10", "10_SHORT_06_00", "short", 6, &[1, 2, 3]);
        trip("10", "10_SHORT_07_00", "short", 7, &[1, 2, 3]);
        trip("10", "10_FULL_08_00", "full", 8, &[1, 2, 3, 4, 5]);
        trip("10", "10_FULL_09_00", "full", 9, &[1, 2, 3, 4, 5]);
        trip("11", "11_FULL_08_30", "down", 8, &[5, 4, 3, 2, 1]);
        let mut shapes = String::from("shape_id,shape_pt_lat,shape_pt_lon,shape_pt_sequence,shape_dist_traveled\n");
        for (id, seq) in [("full", vec![1, 5]), ("short", vec![1, 3]), ("down", vec![5, 1])] {
            for (k, s) in seq.iter().enumerate() {
                shapes += &format!("{id},{},77.2,{},\n", lat(*s), k + 1);
            }
        }
        for (name, text) in [("stops.txt", stops.as_str()), ("routes.txt", routes), ("trips.txt", trips.as_str()), ("stop_times.txt", times.as_str()), ("shapes.txt", shapes.as_str())] {
            std::fs::write(dir.join(name), text).unwrap();
        }
        let net = Network::load(&dir).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        net
    }

    fn search(net: &Network, tt: &Timetable, from: u32, to: u32, depart: u32) -> Vec<crate::raptor::Journey> {
        Search { net, tt, sources: &[(from, 0)], targets: &[(to, 0)], depart, max_rides: 2, banned: &HashSet::new() }.run()
    }

    #[test]
    fn short_turn_trips_become_a_variant_and_the_route_keeps_its_full_pattern() {
        let net = feed();
        let s = |id: &str| net.stop_index[id];
        // Two GTFS routes, then the variant.
        assert_eq!(net.routes.len(), 3);
        let up = net.route_index["10"];
        assert_eq!((up, net.route_index["11"]), (0, 1));
        // The route itself is the five-stop pattern, though a short trip starts earlier and is listed first.
        assert_eq!(net.routes[0].stops, vec![s("s1"), s("s2"), s("s3"), s("s4"), s("s5")]);
        assert_eq!(net.routes[0].starts, vec![8 * 3600, 9 * 3600]);
        assert_eq!(net.routes[0].sched, vec![0, 120, 240, 360, 480]);
        assert!(net.is_main(0) && net.is_main(1) && !net.is_main(2));
        // The variant: same line, first three stops, its own trips, the same segments.
        let v = &net.routes[2];
        assert_eq!((v.id.as_str(), v.name.as_str(), v.main), ("10", "9UP", 0));
        assert_eq!(v.stops, net.routes[0].stops[..3]);
        assert_eq!(v.starts, vec![6 * 3600, 7 * 3600]);
        assert_eq!(v.segs, net.routes[0].segs[..2]);
        assert_eq!(net.variants, vec![vec![2], vec![], vec![]]);
        assert_eq!(net.family(2).collect::<Vec<_>>(), vec![0, 2]);
        // Its shape is its own: about 1.1 km against the route's 2.2 km.
        assert!((v.shape.len() - 1112.0).abs() < 10.0 && (net.routes[0].shape.len() - 2224.0).abs() < 20.0);
        // Segments and per-stop route counts see one route, not two.
        assert_eq!(net.segments.len(), 8);
        assert!(net.segments.iter().all(|g| g.n_routes == 1));
        assert_eq!((net.stop_routes[s("s2") as usize].len(), net.routes_at(s("s2") as usize)), (3, 2));
        // Opposite directions pair up by name between the GTFS routes only.
        assert_eq!((net.sibling(0), net.sibling(1), net.sibling(2)), (Some(1), Some(0), None));
        assert_eq!(net.on_main(2, 2), Some((0, 2)));
        // At stop 2 all four trips call; at stop 4 only the full ones.
        assert_eq!(net.starts_through(0, 1), vec![6 * 3600, 7 * 3600, 8 * 3600, 9 * 3600]);
        assert_eq!(net.starts_through(2, 1), net.starts_through(0, 1));
        assert_eq!(net.starts_through(0, 3), vec![8 * 3600, 9 * 3600]);
        assert_eq!(net.summary(), "5 stops, 2 routes, 5 trips (2 of them short-turn trips, on 1 routes), 8 segments, 0 walking links");
    }

    #[test]
    fn a_short_turn_trip_is_never_ridden_past_its_last_stop() {
        let net = feed();
        let s = |id: &str| net.stop_index[id];
        let tt = Timetable::build(&net, &Scheduled, 0);
        // Leaving stop 1 at 05:55 for stop 3: the 06:00 short trip does it.
        let j = &search(&net, &tt, s("s1"), s("s3"), 5 * 3600 + 3300)[0];
        assert!(matches!(j.legs[..], [Leg::Ride { route: 2, trip: 0, board: 0, alight: 2, .. }]));
        assert_eq!(j.arrive, net.routes[2].starts[0] + (net.segments[0].sched_s + net.segments[1].sched_s).round() as u32);
        // For stop 5 the short trips are no use: the first full trip, at 08:00.
        let j = &search(&net, &tt, s("s1"), s("s5"), 5 * 3600 + 3300)[0];
        assert!(matches!(j.legs[..], [Leg::Ride { route: 0, trip: 0, board: 0, alight: 4, board_time: 28800, .. }]));
    }

    #[test]
    fn live_timetable_drops_past_short_turn_trips_when_the_route_is_tracked() {
        let net = feed();
        let now = Local::parse("2026-11-17T06:30").unwrap().epoch();
        // No bus tracked: the variant keeps both its timetabled trips.
        let tt = Timetable::build_live(&net, &Scheduled, 0, now, &[]);
        assert_eq!(tt.arr[2].len() / 3, 2);
        // A bus of route 10 is on the road (tracked on the route's own entry):
        // the 06:00 short trip, which should already be out, is dropped like a full one would be.
        let states = vec![VehicleState { vehicle: "DL1".into(), route: 0, along: 300.0, ts: now - 10, waiting_s: None }];
        let tt = Timetable::build_live(&net, &Scheduled, 0, now, &states);
        assert_eq!(tt.arr[2].len() / 3, 1);
        assert_eq!(tt.arr[2][0], 7 * 3600);
        assert_eq!(tt.vehicle(0, 0), Some("DL1"));
    }
}
