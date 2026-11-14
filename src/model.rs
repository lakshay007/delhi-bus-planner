//! Historical travel-time model: what a segment usually takes at a given time
//! of day, with graceful fallback where data is thin.
//!
//! The estimate is a product of three learned factors and the segment length:
//!
//!   seconds = length x city_pace(day type, time)      -- how slow Delhi is now
//!                    x segment_ratio(segment)          -- this stretch vs the city
//!                    x bin_ratio(segment, day type, time)  -- its own daily rhythm
//!
//! Each factor is shrunk toward the one above it in proportion to how little
//! data it has, so a segment with thousands of observations is described by
//! its own history and a segment with none inherits its neighbours' ratio and
//! the city's daily profile.

use crate::clock::{DAY_KINDS, Local};
use crate::gtfs::Network;
use crate::raptor::SegTimes;
use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use std::io::{Read, Write};
use std::path::Path;

/// 30-minute bins.
pub const BINS: usize = 48;
const BIN_S: f32 = 1800.0;
/// Observations needed before a cell is reported as estimated from its own data.
const LEVEL_N: f32 = 8.0;
/// Segments shorter than this are too short to time; they get the city pace.
const MIN_LEN_M: f32 = 30.0;
/// Pace limits, s/m: 90 km/h to 1.4 km/h.
const PACE: (f32, f32) = (0.04, 2.5);
/// Pace used when there is no data at all: 18 km/h, the timetable's speed.
const DEFAULT_PACE: f32 = 0.2;

/// The model's settings. The defaults are the ones chosen on the October
/// 2026 archive; `busplan tune` compares alternatives.
#[derive(Clone, Debug)]
pub struct Params {
    /// Pseudo-observations pulling a city bin toward the all-day city pace.
    pub k_city: f32,
    /// Pseudo-observations pulling a segment toward its prior.
    pub k_seg: f32,
    /// Pseudo-observations pulling a segment's time-of-day bin toward "no deviation".
    pub k_bin: f32,
    /// A single observation may say a segment is at most this many times
    /// faster or slower than expected. Limits the pull of one stuck bus.
    pub cap: (f32, f32),
    /// Weight of the half-hours either side when estimating a half-hour.
    pub nb_w: f32,
    /// Day kind (clock::Local::day_type) -> the class it is pooled into.
    pub day_map: [u8; DAY_KINDS],
    /// An observation this many days old counts half. 0 = all days count equally.
    pub half_life_days: f32,
    /// Interpolate between half-hour centres instead of stepping.
    pub interp: bool,
    /// Rescale each day class and half-hour so that, over the training data,
    /// predicted time adds up to observed time. Capping single observations
    /// keeps one stuck bus from distorting a segment but trims real slow
    /// passes too, which leaves every estimate a little short, most of all in
    /// the peaks; this puts the total back without un-capping any segment.
    pub calibrate: bool,
}

impl Default for Params {
    fn default() -> Self {
        Params { k_city: 300.0, k_seg: 8.0, k_bin: 16.0, cap: (0.33, 3.0), nb_w: 0.5, day_map: [0, 1, 2, 3], half_life_days: 0.0, interp: true, calibrate: false }
    }
}

/// One observed traversal.
#[derive(Clone, Copy, Debug)]
pub struct Obs {
    pub seg: u32,
    pub t_enter: i64,
    pub dur: f32,
}

/// One value per day class and half-hour.
type Grid<T> = [[T; BINS]; DAY_KINDS];

pub struct Model {
    /// City-wide pace, s/m, by day class and bin.
    city: Grid<f32>,
    city_n: Grid<u32>,
    seg_ratio: Vec<f32>,
    pub seg_n: Vec<u32>,
    /// 4 = own data, 5 = neighbours' ratio, 6 = city only.
    seg_src: Vec<u8>,
    bin_ratio: Vec<Option<Box<Grid<f32>>>>,
    bin_n: Vec<Option<Box<Grid<u16>>>>,
    /// (day class, bin) with the fastest city pace: the quiet-hour reference.
    quiet: (usize, usize),
    day_map: [u8; DAY_KINDS],
    interp: bool,
    pub n_obs: usize,
    pub first_obs: i64,
    pub last_obs: i64,
}

fn bin_of(tod: u32) -> usize {
    ((tod % 86_400) as usize / 1800) % BINS
}

impl Model {
    pub fn fit(net: &Network, obs: &[Obs]) -> Model {
        Self::fit_with(net, obs, &Params::default())
    }

    pub fn fit_with(net: &Network, obs: &[Obs], p: &Params) -> Model {
        let ns = net.segments.len();
        let len = |s: u32| net.segments[s as usize].len_m;
        let t_last = obs.iter().map(|o| o.t_enter).max().unwrap_or(0);
        // (segment, day class, bin, pace, weight)
        let usable: Vec<(u32, u8, u8, f32, f32)> = obs
            .par_iter()
            .filter_map(|o| {
                let l = len(o.seg);
                let pace = o.dur / l;
                if l < MIN_LEN_M || !(PACE.0..=PACE.1).contains(&pace) {
                    return None;
                }
                let t = Local::from_epoch(o.t_enter);
                let w = if p.half_life_days > 0.0 { 0.5f32.powf((t_last - o.t_enter) as f32 / 86_400.0 / p.half_life_days) } else { 1.0 };
                Some((o.seg, p.day_map[t.day_type()], bin_of(t.tod) as u8, pace, w))
            })
            .collect();
        let nb = p.nb_w as f64;
        let prev = |b: usize| (b + BINS - 1) % BINS;
        let next = |b: usize| (b + 1) % BINS;

        // ---- 1. city pace: total seconds / total metres per cell
        let mut dur: Grid<f64> = [[0.0; BINS]; DAY_KINDS];
        let mut met: Grid<f64> = [[0.0; BINS]; DAY_KINDS];
        let mut cw: Grid<f64> = [[0.0; BINS]; DAY_KINDS];
        let mut city_n: Grid<u32> = [[0; BINS]; DAY_KINDS];
        for &(s, c, b, pace, w) in &usable {
            let (c, b, l, w) = (c as usize, b as usize, len(s) as f64, w as f64);
            dur[c][b] += w * pace as f64 * l;
            met[c][b] += w * l;
            cw[c][b] += w;
            city_n[c][b] += 1;
        }
        let (td, tm): (f64, f64) = (dur.iter().flatten().sum(), met.iter().flatten().sum());
        let c0 = if tm > 0.0 { (td / tm) as f32 } else { DEFAULT_PACE };
        // Neighbouring bins lend part-weight evidence. Smoothing the sums (not
        // the paces) means an empty neighbour contributes nothing.
        let sm = |g: &Grid<f64>, c: usize, b: usize| g[c][b] + nb * (g[c][prev(b)] + g[c][next(b)]);
        let k_city = p.k_city.max(1e-6);
        let mut city: Grid<f32> = [[c0; BINS]; DAY_KINDS];
        for b in 0..BINS {
            let (d, m, n) = (0..DAY_KINDS).fold((0.0, 0.0, 0.0), |a, c| (a.0 + sm(&dur, c, b), a.1 + sm(&met, c, b), a.2 + sm(&cw, c, b)));
            let n = n as f32;
            let all = if m > 0.0 { (n * (d / m) as f32 + k_city * c0) / (n + k_city) } else { c0 };
            for c in 0..DAY_KINDS {
                let (d, m, n) = (sm(&dur, c, b), sm(&met, c, b), sm(&cw, c, b) as f32);
                let x = if m > 0.0 { (d / m) as f32 } else { all };
                city[c][b] = (n * x + k_city * all) / (n + k_city);
            }
        }

        // ---- 2. segment ratio: how this segment compares with the city at the same time
        let mut rs = vec![0.0f64; ns];
        let mut seg_w = vec![0.0f64; ns];
        let mut seg_n = vec![0u32; ns];
        for &(s, c, b, pace, w) in &usable {
            rs[s as usize] += w as f64 * (pace / city[c as usize][b as usize]).clamp(p.cap.0, p.cap.1) as f64;
            seg_w[s as usize] += w as f64;
            seg_n[s as usize] += 1;
        }
        let k_seg = p.k_seg.max(1e-6);
        // Estimates for observed segments, then spread to unobserved neighbours.
        let mut est: Vec<Option<f32>> = (0..ns)
            .map(|s| (seg_n[s] >= 3).then(|| ((rs[s] as f32) + k_seg) / (seg_w[s] as f32 + k_seg)))
            .collect();
        let mut seg_ratio = vec![1.0f32; ns];
        let mut seg_src = vec![6u8; ns];
        let mut prior = vec![1.0f32; ns];
        for pass in 0..3 {
            let snapshot = est.clone();
            for s in 0..ns {
                let nbrs: Vec<f32> = net.seg_neighbours[s].iter().filter_map(|&o| snapshot[o as usize]).collect();
                if nbrs.is_empty() {
                    continue;
                }
                let m = nbrs.len() as f32;
                let pr = (nbrs.iter().sum::<f32>() + 2.0) / (m + 2.0);
                if pass == 0 {
                    prior[s] = pr;
                    if seg_src[s] == 6 {
                        seg_src[s] = 5;
                    }
                }
                if est[s].is_none() {
                    est[s] = Some(pr);
                    prior[s] = pr;
                    seg_src[s] = 5;
                }
            }
        }
        for s in 0..ns {
            seg_ratio[s] = ((rs[s] as f32) + k_seg * prior[s]) / (seg_w[s] as f32 + k_seg);
            if seg_n[s] as f32 >= LEVEL_N {
                seg_src[s] = 4;
            }
        }

        // ---- 3. per-segment time-of-day deviation from city x segment ratio
        let mut sums: Vec<Option<Box<Grid<f32>>>> = (0..ns).map(|_| None).collect();
        let mut wts: Vec<Option<Box<Grid<f32>>>> = (0..ns).map(|_| None).collect();
        let mut bin_n: Vec<Option<Box<Grid<u16>>>> = (0..ns).map(|_| None).collect();
        for &(s, c, b, pace, w) in &usable {
            let (su, c, b) = (s as usize, c as usize, b as usize);
            let q = (pace / (city[c][b] * seg_ratio[su])).clamp(p.cap.0, p.cap.1);
            sums[su].get_or_insert_with(|| Box::new([[0.0; BINS]; DAY_KINDS]))[c][b] += w * q;
            wts[su].get_or_insert_with(|| Box::new([[0.0; BINS]; DAY_KINDS]))[c][b] += w;
            let n = &mut bin_n[su].get_or_insert_with(|| Box::new([[0; BINS]; DAY_KINDS]))[c][b];
            *n = n.saturating_add(1);
        }
        let k_bin = p.k_bin.max(1e-6);
        let mut bin_ratio: Vec<Option<Box<Grid<f32>>>> = (0..ns).map(|_| None).collect();
        for s in 0..ns {
            let (Some(sm), Some(cn)) = (&sums[s], &wts[s]) else { continue };
            let mut out = Box::new([[1.0f32; BINS]; DAY_KINDS]);
            let sm_s = |c: usize, b: usize| sm[c][b] + p.nb_w * (sm[c][prev(b)] + sm[c][next(b)]);
            let cn_s = |c: usize, b: usize| cn[c][b] + p.nb_w * (cn[c][prev(b)] + cn[c][next(b)]);
            for b in 0..BINS {
                let (num, den) = (0..DAY_KINDS).fold((0.0, 0.0), |a, c| (a.0 + sm_s(c, b), a.1 + cn_s(c, b)));
                let all = (num + k_bin) / (den + k_bin);
                for c in 0..DAY_KINDS {
                    out[c][b] = (sm_s(c, b) + k_bin * all) / (cn_s(c, b) + k_bin);
                }
            }
            bin_ratio[s] = Some(out);
        }

        // ---- 4. calibration: make totals match per day class and half-hour
        if p.calibrate {
            let mut act: Grid<f64> = [[0.0; BINS]; DAY_KINDS];
            let mut prd: Grid<f64> = [[0.0; BINS]; DAY_KINDS];
            for &(s, c, b, pace, w) in &usable {
                let (su, c, b, l) = (s as usize, c as usize, b as usize, len(s));
                let r = bin_ratio[su].as_ref().map_or(1.0, |g| g[c][b]);
                act[c][b] += (w * pace * l) as f64;
                prd[c][b] += (w * Self::clamp(l, l * city[c][b] * seg_ratio[su] * r)) as f64;
            }
            for c in 0..DAY_KINDS {
                for b in 0..BINS {
                    if city_n[c][b] >= 1000 && prd[c][b] > 0.0 {
                        city[c][b] *= (act[c][b] / prd[c][b]).clamp(0.9, 1.25) as f32;
                    }
                }
            }
        }

        // ---- quiet-hour reference: fastest well-observed city bin
        let mut quiet = (0, 10); // 05:00 weekday if nothing better
        let mut best = f32::INFINITY;
        for c in 0..DAY_KINDS {
            for b in 0..BINS {
                if city_n[c][b] >= 200 && city[c][b] < best {
                    best = city[c][b];
                    quiet = (c, b);
                }
            }
        }

        Model {
            city,
            city_n,
            seg_ratio,
            seg_n,
            seg_src,
            bin_ratio,
            bin_n,
            quiet,
            day_map: p.day_map,
            interp: p.interp,
            n_obs: usable.len(),
            first_obs: obs.iter().map(|o| o.t_enter).min().unwrap_or(0),
            last_obs: t_last,
        }
    }

    /// The class a day kind is pooled into.
    fn class(&self, day_type: usize) -> usize {
        self.day_map[day_type.min(DAY_KINDS - 1)] as usize
    }

    fn at_bin(&self, seg: usize, dt: usize, b: usize) -> f32 {
        self.city[dt][b] * self.bin_ratio[seg].as_ref().map_or(1.0, |g| g[dt][b])
    }

    fn clamp(len: f32, secs: f32) -> f32 {
        secs.clamp(len * PACE.0, len * PACE.1).max(1.0)
    }

    /// Write the fitted model to a gzipped file, so a server can start from
    /// it without the traversal data. Segments are stored under their two
    /// stop ids, so the file also loads on a slightly different cut of the
    /// same GTFS.
    pub fn save(&self, net: &Network, path: &Path) -> Result<()> {
        let mut b: Vec<u8> = Vec::new();
        b.extend_from_slice(MODEL_MAGIC);
        b.extend((self.n_obs as u64).to_le_bytes());
        b.extend(self.first_obs.to_le_bytes());
        b.extend(self.last_obs.to_le_bytes());
        b.extend([self.quiet.0 as u8, self.quiet.1 as u8, self.interp as u8]);
        b.extend(self.day_map);
        for g in &self.city {
            g.iter().for_each(|v| b.extend(v.to_le_bytes()));
        }
        for g in &self.city_n {
            g.iter().for_each(|v| b.extend(v.to_le_bytes()));
        }
        b.extend((net.segments.len() as u32).to_le_bytes());
        for (s, seg) in net.segments.iter().enumerate() {
            for id in [&net.stops[seg.a as usize].id, &net.stops[seg.b as usize].id] {
                b.extend((id.len() as u16).to_le_bytes());
                b.extend(id.as_bytes());
            }
            b.extend(self.seg_ratio[s].to_le_bytes());
            b.extend(self.seg_n[s].to_le_bytes());
            b.push(self.seg_src[s]);
            b.push(self.bin_ratio[s].is_some() as u8 | (self.bin_n[s].is_some() as u8) << 1);
            if let Some(g) = &self.bin_ratio[s] {
                g.iter().flatten().for_each(|v| b.extend(v.to_le_bytes()));
            }
            if let Some(g) = &self.bin_n[s] {
                g.iter().flatten().for_each(|v| b.extend(v.to_le_bytes()));
            }
        }
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        let mut gz = flate2::write::GzEncoder::new(std::fs::File::create(&tmp)?, flate2::Compression::best());
        gz.write_all(&b)?;
        gz.finish()?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Read a model written by `save`. Returns the model and how many of this
    /// network's segments the file had. A segment the file does not know
    /// takes its neighbours' ratio, or the city pace alone, exactly as an
    /// unobserved segment does in a fresh fit. Fails if fewer than half match:
    /// that is a different network, not a different cut of this one.
    pub fn load(net: &Network, path: &Path) -> Result<(Model, usize)> {
        let mut b = Vec::new();
        flate2::read::GzDecoder::new(std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?)
            .read_to_end(&mut b)
            .with_context(|| format!("reading {}", path.display()))?;
        let mut r = Bytes { b: &b, i: 0 };
        if r.take(MODEL_MAGIC.len())? != MODEL_MAGIC {
            bail!("{} is not a busplan model file (or is from another version)", path.display());
        }
        let (n_obs, first_obs, last_obs) = (r.u64()? as usize, r.u64()? as i64, r.u64()? as i64);
        let (q0, q1, interp) = (r.u8()? as usize, r.u8()? as usize, r.u8()? != 0);
        let mut day_map = [0u8; DAY_KINDS];
        day_map.copy_from_slice(r.take(DAY_KINDS)?);
        if q0 >= DAY_KINDS || q1 >= BINS || day_map.iter().any(|&c| c as usize >= DAY_KINDS) {
            bail!("{} is damaged", path.display());
        }
        let mut city: Grid<f32> = [[0.0; BINS]; DAY_KINDS];
        for v in city.iter_mut().flatten() {
            *v = r.f32()?;
        }
        let mut city_n: Grid<u32> = [[0; BINS]; DAY_KINDS];
        for v in city_n.iter_mut().flatten() {
            *v = r.u32()?;
        }
        let ns = net.segments.len();
        let (mut seg_ratio, mut seg_n, mut seg_src) = (vec![1.0f32; ns], vec![0u32; ns], vec![6u8; ns]);
        let mut bin_ratio: Vec<Option<Box<Grid<f32>>>> = (0..ns).map(|_| None).collect();
        let mut bin_n: Vec<Option<Box<Grid<u16>>>> = (0..ns).map(|_| None).collect();
        let mut matched = vec![false; ns];
        for _ in 0..r.u32()? {
            let stop = |r: &mut Bytes| -> Result<Option<u32>> {
                let n = r.u16()? as usize;
                let id = std::str::from_utf8(r.take(n)?).context("model file is damaged")?;
                Ok(net.stop_index.get(id).copied())
            };
            let (a, bb) = (stop(&mut r)?, stop(&mut r)?);
            let (ratio, n, src, flags) = (r.f32()?, r.u32()?, r.u8()?, r.u8()?);
            let gr = if flags & 1 != 0 {
                let mut g: Box<Grid<f32>> = Box::new([[0.0; BINS]; DAY_KINDS]);
                for v in g.iter_mut().flatten() {
                    *v = r.f32()?;
                }
                Some(g)
            } else {
                None
            };
            let gn = if flags & 2 != 0 {
                let mut g: Box<Grid<u16>> = Box::new([[0; BINS]; DAY_KINDS]);
                for v in g.iter_mut().flatten() {
                    *v = r.u16()?;
                }
                Some(g)
            } else {
                None
            };
            if let Some(&s) = a.zip(bb).and_then(|k| net.seg_index.get(&k)) {
                let s = s as usize;
                (seg_ratio[s], seg_n[s], seg_src[s], matched[s]) = (ratio, n, src, true);
                bin_ratio[s] = gr;
                bin_n[s] = gn;
            }
        }
        if r.i != b.len() {
            bail!("{} has trailing data; it is damaged or from another version", path.display());
        }
        let n_matched = matched.iter().filter(|&&m| m).count();
        if n_matched * 2 < ns {
            bail!(
                "{} knows only {n_matched} of this GTFS's {ns} stop-to-stop segments: it was fitted on a different network. Refit with `busplan save-model`.",
                path.display()
            );
        }
        // Segments the file does not know: neighbours' ratio if any has data, else the city pace alone.
        for s in (0..ns).filter(|&s| !matched[s]) {
            let nb: Vec<f32> = net.seg_neighbours[s].iter().filter(|&&o| matched[o as usize] && seg_n[o as usize] > 0).map(|&o| seg_ratio[o as usize]).collect();
            if !nb.is_empty() {
                (seg_ratio[s], seg_src[s]) = (nb.iter().sum::<f32>() / nb.len() as f32, 5);
            }
        }
        Ok((Model { city, city_n, seg_ratio, seg_n, seg_src, bin_ratio, bin_n, quiet: (q0, q1), day_map, interp, n_obs, first_obs, last_obs }, n_matched))
    }

    pub fn segments_with_data(&self) -> usize {
        self.seg_n.iter().filter(|&&n| n > 0).count()
    }

    /// City-wide average speed in km/h for each bin of a day class, for display.
    pub fn city_speed_kmh(&self, class: usize) -> Vec<f32> {
        self.city[class].iter().map(|p| 3.6 / p).collect()
    }

    pub fn city_count(&self, class: usize) -> Vec<u32> {
        self.city_n[class].to_vec()
    }

    /// The day classes in use, each with the names of the day kinds pooled into it.
    pub fn classes(&self) -> Vec<(usize, String)> {
        let mut out: Vec<(usize, String)> = Vec::new();
        for (kind, &c) in self.day_map.iter().enumerate() {
            match out.iter_mut().find(|x| x.0 == c as usize) {
                Some(x) => x.1 = format!("{}+{}", x.1, crate::clock::DAY_KIND_NAMES[kind]),
                None => out.push((c as usize, crate::clock::DAY_KIND_NAMES[kind].to_string())),
            }
        }
        out
    }
}

/// First bytes of a saved model. The digit is the format version.
const MODEL_MAGIC: &[u8] = b"BUSPLAN-MODEL-1\n";

/// Little-endian reader over a byte slice.
struct Bytes<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Bytes<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.i.checked_add(n).filter(|&e| e <= self.b.len()).context("model file is truncated")?;
        let s = &self.b[self.i..end];
        self.i = end;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn f32(&mut self) -> Result<f32> {
        Ok(f32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
}

impl SegTimes for Model {
    fn secs(&self, net: &Network, seg: u32, day_type: usize, tod: u32) -> f32 {
        let s = seg as usize;
        let day_type = self.class(day_type);
        let len = net.segments[s].len_m;
        let v = if self.interp {
            // Interpolate between bin centres so estimates change smoothly with time.
            let p = (tod % 86_400) as f32 / BIN_S - 0.5;
            let b0 = (p.floor() as i32).rem_euclid(BINS as i32) as usize;
            let f = p - p.floor();
            (1.0 - f) * self.at_bin(s, day_type, b0) + f * self.at_bin(s, day_type, (b0 + 1) % BINS)
        } else {
            self.at_bin(s, day_type, bin_of(tod))
        };
        let ratio = if len < MIN_LEN_M { 1.0 } else { self.seg_ratio[s] };
        Self::clamp(len, len * v * ratio)
    }

    fn quiet_secs(&self, net: &Network, seg: u32) -> f32 {
        let s = seg as usize;
        let len = net.segments[s].len_m;
        let ratio = if len < MIN_LEN_M { 1.0 } else { self.seg_ratio[s] };
        Self::clamp(len, len * self.at_bin(s, self.quiet.0, self.quiet.1) * ratio)
    }

    fn level(&self, seg: u32, day_type: usize, tod: u32) -> u8 {
        let s = seg as usize;
        let b = bin_of(tod);
        if let Some(n) = &self.bin_n[s] {
            if n[self.class(day_type)][b] as f32 >= LEVEL_N {
                return 1;
            }
            if n.iter().map(|g| g[b] as f32).sum::<f32>() >= LEVEL_N {
                return 2;
            }
        }
        self.seg_src[s]
    }

    fn name(&self) -> &'static str {
        "model"
    }
}

/// Read traversal CSVs written by `extract`. `keep` filters by IST date
/// string ("2026-11-14"), which is how train/test splits are made.
pub fn load_traversals(net: &Network, dir: &Path, keep: impl Fn(&str) -> bool) -> Result<Vec<Obs>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut files: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_suffix(".csv")).is_some_and(|d| !d.starts_with('.') && keep(d)))
        .collect();
    files.sort();
    // One thread per day file: a month of archive is a few gigabytes of CSV.
    let parts: Vec<Vec<Obs>> = files
        .par_iter()
        .map(|p| -> Result<_> {
            let mut out = Vec::new();
            let mut rd = csv::Reader::from_path(p).with_context(|| format!("opening {}", p.display()))?;
            let mut r = csv::StringRecord::new();
            while rd.read_record(&mut r)? {
                let (Some(&a), Some(&b)) = (net.stop_index.get(&r[2]), net.stop_index.get(&r[3])) else { continue };
                let Some(&seg) = net.seg_index.get(&(a, b)) else { continue };
                out.push(Obs { seg, t_enter: r[0].parse::<f64>()? as i64, dur: r[1].parse()? });
            }
            Ok(out)
        })
        .collect::<Result<_>>()?;
    Ok(parts.concat())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::Polyline;
    use crate::gtfs::{Route, Segment, Stop};
    use std::collections::HashMap;

    /// Three 500 m segments in a row.
    fn toy() -> Network {
        let stops: Vec<Stop> =
            (0..4).map(|i| Stop { id: format!("s{i}"), name: String::new(), lat: 0.0, lon: 0.0, xy: (i as f64 * 500.0, 0.0) }).collect();
        let segments: Vec<Segment> =
            (0..3).map(|i| Segment { a: i, b: i + 1, len_m: 500.0, sched_s: 100.0, n_routes: 1 }).collect();
        let route = Route {
            id: "r".into(),
            name: "1UP".into(),
            desc: String::new(),
            agency: String::new(),
            stops: (0..4).collect(),
            sched: vec![0, 100, 200, 300],
            starts: vec![0],
            shape: Polyline::new(vec![(0.0, 0.0), (1500.0, 0.0)]),
            along: vec![0.0, 500.0, 1000.0, 1500.0],
            segs: vec![0, 1, 2],
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
            seg_neighbours: vec![vec![1], vec![0, 2], vec![1]],
            siblings: vec![],
        }
    }

    /// Epoch for a weekday (Tue 2026-11-17) at the given hour, IST.
    fn at(hour: u32) -> i64 {
        Local::parse("2026-11-17T00:00").unwrap().epoch() + hour as i64 * 3600 + 900
    }

    #[test]
    fn learns_peak_and_offpeak_and_fills_unobserved_segment() {
        let net = toy();
        let mut obs = Vec::new();
        // Segments 0 and 1: 60 s off-peak (05:00-06:00), 150 s in the 09:00 peak. Segment 2: never observed.
        for seg in 0..2 {
            for _ in 0..400 {
                obs.push(Obs { seg, t_enter: at(5), dur: 60.0 });
                obs.push(Obs { seg, t_enter: at(9), dur: 150.0 });
            }
        }
        let m = Model::fit(&net, &obs);
        let off = m.secs(&net, 0, 0, 5 * 3600 + 900);
        let peak = m.secs(&net, 0, 0, 9 * 3600 + 900);
        assert!((off - 60.0).abs() < 6.0, "off-peak {off}");
        assert!((peak - 150.0).abs() < 15.0, "peak {peak}");
        assert_eq!(m.level(0, 0, 9 * 3600 + 900), 1);
        // The unobserved segment follows the city's rhythm via its neighbours.
        let (off2, peak2) = (m.secs(&net, 2, 0, 5 * 3600 + 900), m.secs(&net, 2, 0, 9 * 3600 + 900));
        assert!(peak2 > off2 * 1.8, "unobserved segment should still show a peak: {off2} vs {peak2}");
        assert_eq!(m.level(2, 0, 9 * 3600 + 900), 5);
        // Quiet-hour reference is the off-peak value.
        assert!((m.quiet_secs(&net, 0) - 60.0).abs() < 6.0);
    }

    #[test]
    fn each_kind_of_day_has_its_own_profile_and_thin_ones_borrow_from_all_days() {
        let net = toy();
        let day = |d: &str| Local::parse(&format!("{d}T12:15")).unwrap().epoch();
        let (tue, sat, sun, holiday) = (day("2026-11-17"), day("2026-11-14"), day("2026-11-15"), day("2026-10-20"));
        let mut obs = Vec::new();
        for _ in 0..400 {
            obs.push(Obs { seg: 0, t_enter: tue, dur: 150.0 });
            obs.push(Obs { seg: 0, t_enter: sat, dur: 120.0 });
            obs.push(Obs { seg: 0, t_enter: sun, dur: 80.0 });
        }
        // A public holiday (a Tuesday) with only a few buses.
        for _ in 0..4 {
            obs.push(Obs { seg: 0, t_enter: holiday, dur: 60.0 });
        }
        let m = Model::fit(&net, &obs);
        let at = |t: i64| {
            let l = Local::from_epoch(t);
            m.secs(&net, 0, l.day_type(), l.tod)
        };
        assert!((at(tue) - 150.0).abs() < 8.0 && (at(sat) - 120.0).abs() < 8.0 && (at(sun) - 80.0).abs() < 8.0, "{} {} {}", at(tue), at(sat), at(sun));
        // Four holiday buses pull the estimate below the all-days average, not all the way to 60 s.
        let h = at(holiday);
        assert!(h > 75.0 && h < 115.0, "holiday {h}");
        assert_eq!(m.level(0, Local::from_epoch(holiday).day_type(), 12 * 3600 + 900), 2);
    }

    #[test]
    fn a_saved_model_gives_the_same_answers_and_refuses_another_network() {
        let mut net = toy();
        net.stop_index = net.stops.iter().enumerate().map(|(i, s)| (s.id.clone(), i as u32)).collect();
        net.seg_index = net.segments.iter().enumerate().map(|(i, s)| ((s.a, s.b), i as u32)).collect();
        let mut obs = Vec::new();
        for i in 0..300 {
            obs.push(Obs { seg: 0, t_enter: at(8), dur: 150.0 + (i % 7) as f32 });
            obs.push(Obs { seg: 0, t_enter: at(14), dur: 90.0 });
            obs.push(Obs { seg: 1, t_enter: at(8), dur: 120.0 });
        }
        let m = Model::fit(&net, &obs);
        let path = std::env::temp_dir().join(format!("busplan-model-test-{}.gz", std::process::id()));
        m.save(&net, &path).unwrap();
        let (l, matched) = Model::load(&net, &path).unwrap();
        assert_eq!(matched, 3);
        for seg in 0..3u32 {
            for day in 0..DAY_KINDS {
                for tod in (0..86_400).step_by(900) {
                    assert_eq!(m.secs(&net, seg, day, tod), l.secs(&net, seg, day, tod));
                    assert_eq!(m.level(seg, day, tod), l.level(seg, day, tod));
                }
            }
            assert_eq!(m.quiet_secs(&net, seg), l.quiet_secs(&net, seg));
        }
        assert_eq!((m.n_obs, m.first_obs, m.last_obs, m.segments_with_data()), (l.n_obs, l.first_obs, l.last_obs, l.segments_with_data()));
        // A network where one stop was renamed: segments 0 and 1 are unknown to the file,
        // segment 2 still loads. Fewer than half match, so it is refused...
        let renamed = |ids: &[(usize, &str)]| {
            let mut n = toy();
            for &(i, id) in ids {
                n.stops[i].id = id.into();
            }
            n.stop_index = n.stops.iter().enumerate().map(|(i, s)| (s.id.clone(), i as u32)).collect();
            n.seg_index = n.segments.iter().enumerate().map(|(i, s)| ((s.a, s.b), i as u32)).collect();
            n
        };
        assert!(Model::load(&renamed(&[(1, "elsewhere")]), &path).is_err());
        // ...but with only the last stop renamed, two of three match and the third
        // takes its neighbour's ratio.
        let other = renamed(&[(3, "elsewhere")]);
        let (o, matched) = Model::load(&other, &path).unwrap();
        assert_eq!(matched, 2);
        assert_eq!(o.secs(&other, 0, 0, 8 * 3600), m.secs(&net, 0, 0, 8 * 3600));
        assert_eq!(o.level(2, 0, 8 * 3600), 5);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn no_data_falls_back_to_timetable_speed() {
        let net = toy();
        let m = Model::fit(&net, &[]);
        // 500 m at 18 km/h = 100 s.
        assert!((m.secs(&net, 0, 0, 12 * 3600) - 100.0).abs() < 0.5);
        assert_eq!(m.level(0, 0, 12 * 3600), 6);
    }

    #[test]
    fn a_few_outliers_do_not_dominate_a_sparse_segment() {
        let net = toy();
        let mut obs = Vec::new();
        for _ in 0..500 {
            obs.push(Obs { seg: 0, t_enter: at(12), dur: 100.0 });
        }
        // Two wild observations on segment 1.
        obs.push(Obs { seg: 1, t_enter: at(12), dur: 600.0 });
        obs.push(Obs { seg: 1, t_enter: at(12), dur: 600.0 });
        let m = Model::fit(&net, &obs);
        let v = m.secs(&net, 1, 0, 12 * 3600 + 900);
        assert!(v < 250.0, "two outliers should be shrunk hard toward the neighbour: {v}");
    }
}
