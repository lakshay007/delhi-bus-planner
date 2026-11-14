//! Load the static GTFS and build the network everything else works on.
//!
//! Facts about this feed that the loader relies on (measured):
//! a route_id is one direction with one shape and one stop pattern, and every
//! trip of a route has identical stop offsets. The loader verifies both and
//! reports violations instead of silently assuming.

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
        let mut route_shape: Vec<Option<String>> = vec![None; meta.len()];
        for r in rd.records() {
            let r = r?;
            let ri = *route_index.get(&r[cr]).ok_or_else(|| anyhow!("trip {} on unknown route {}", &r[ct], &r[cr]))?;
            trip_route.insert(r[ct].to_string(), ri);
            let s = &mut route_shape[ri as usize];
            match s {
                None => *s = Some(r[csh].to_string()),
                Some(x) if x != &r[csh] => eprintln!("warning: route {} has more than one shape; using the first", &r[cr]),
                _ => {}
            }
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

        // ---- assemble routes
        let mut routes = Vec::with_capacity(meta.len());
        let mut dropped = 0;
        let mut irregular = 0;
        let mut old_to_new: Vec<Option<u32>> = vec![None; meta.len()];
        for (ri, (id, name, desc, agency)) in meta.into_iter().enumerate() {
            let mut trips: Vec<Vec<(u32, u32, u32)>> = std::mem::take(&mut per_route[ri]).into_values().collect();
            for t in trips.iter_mut() {
                t.sort_unstable();
            }
            trips.retain(|t| t.len() >= 2);
            let Some(first) = trips.iter().min_by_key(|t| t[0].2) else {
                dropped += 1;
                continue;
            };
            let stop_seq: Vec<u32> = first.iter().map(|x| x.1).collect();
            let sched: Vec<u32> = first.iter().map(|x| x.2 - first[0].2).collect();
            // Verify the "one pattern, identical offsets" assumption.
            for t in &trips {
                let same = t.len() == stop_seq.len()
                    && t.iter().zip(&stop_seq).all(|(a, b)| a.1 == *b)
                    && t.iter().zip(&sched).all(|(a, off)| (a.2 - t[0].2).abs_diff(*off) <= 2);
                if !same {
                    irregular += 1;
                }
            }
            let mut starts: Vec<u32> = trips.iter().map(|t| t[0].2).collect();
            starts.sort_unstable();
            let Some(mut pts) = route_shape[ri].as_ref().and_then(|s| shape_pts.get(s)).cloned() else {
                dropped += 1;
                continue;
            };
            pts.sort_by_key(|p| p.0);
            let shape = Polyline::new(pts.into_iter().map(|p| p.1).collect());
            let stop_xy: Vec<P> = stop_seq.iter().map(|&s| stops[s as usize].xy).collect();
            let along = monotone_project(&shape, &stop_xy);
            old_to_new[ri] = Some(routes.len() as u32);
            routes.push(Route { id, name, desc, agency, stops: stop_seq, sched, starts, shape, along, segs: vec![] });
        }
        if dropped > 0 {
            eprintln!("warning: {dropped} routes dropped (no usable trips or shape)");
        }
        if irregular > 0 {
            eprintln!("warning: {irregular} trips do not follow their route's stop pattern/offsets; pattern of the first trip is used");
        }
        let route_index: HashMap<String, u32> =
            route_index.into_iter().filter_map(|(k, v)| old_to_new[v as usize].map(|n| (k, n))).collect();

        // ---- segments
        let mut seg_index: HashMap<(u32, u32), u32> = HashMap::new();
        let mut acc: Vec<(u32, u32, f64, f64, u32)> = Vec::new(); // a, b, sum_len, sched, n
        for r in routes.iter_mut() {
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
                acc[id as usize].2 += len;
                acc[id as usize].4 += 1;
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

        let by_name: HashMap<&str, u32> = routes.iter().enumerate().map(|(i, r)| (r.name.as_str(), i as u32)).collect();
        let siblings: Vec<Option<u32>> = routes
            .iter()
            .map(|r| {
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

        Ok(Network { stops, routes, segments, stop_index, route_index, seg_index, stop_routes, footpaths, seg_neighbours, siblings })
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
        format!(
            "{} stops, {} routes, {} trips, {} segments, {} walking links",
            self.stops.len(),
            self.routes.len(),
            trips,
            self.segments.len(),
            fp
        )
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
            let key = (q.len() - hit, t.len().saturating_sub(hit), usize::MAX - self.stop_routes[i].len(), s.name.as_str());
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
                e.1 += self.stop_routes[i].len();
                e.4 += 1;
            }
        }
        let mut v: Vec<_> = seen.into_iter().collect();
        v.sort_by(|a, b| (a.1.0, usize::MAX - a.1.1).cmp(&(b.1.0, usize::MAX - b.1.1)).then(a.0.cmp(&b.0)));
        v.into_iter().take(limit).map(|(n, e)| (n, e.2, e.3, e.1)).collect()
    }
}
