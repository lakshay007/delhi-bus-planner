//! Read archived GTFS-Realtime VehiclePositions snapshots.
//!
//! A hand-written protobuf reader for the handful of fields this feed uses,
//! so no protoc or generated code is needed. Field numbers are from the
//! GTFS-Realtime spec; the layout was checked against the live feed.

use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default)]
pub struct Veh {
    pub id: String,
    pub route: String,
    /// Dispatch record like "1913_01_02_695". Does not join to trips.txt.
    pub trip: String,
    pub lat: f32,
    pub lon: f32,
    /// Device timestamp, epoch seconds.
    pub ts: i64,
}

struct Rd<'a> {
    b: &'a [u8],
    i: usize,
}

enum Val<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Fixed32([u8; 4]),
    Fixed64,
}

impl<'a> Rd<'a> {
    fn new(b: &'a [u8]) -> Self {
        Rd { b, i: 0 }
    }

    fn varint(&mut self) -> Result<u64> {
        let (mut x, mut s) = (0u64, 0);
        loop {
            let Some(&c) = self.b.get(self.i) else { bail!("truncated varint") };
            self.i += 1;
            x |= ((c & 0x7f) as u64) << s;
            if c & 0x80 == 0 {
                return Ok(x);
            }
            s += 7;
            if s > 63 {
                bail!("varint too long");
            }
        }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.i.checked_add(n).filter(|&e| e <= self.b.len()).context("truncated field")?;
        let s = &self.b[self.i..end];
        self.i = end;
        Ok(s)
    }

    fn next(&mut self) -> Result<Option<(u64, Val<'a>)>> {
        if self.i >= self.b.len() {
            return Ok(None);
        }
        let key = self.varint()?;
        let v = match key & 7 {
            0 => Val::Varint(self.varint()?),
            1 => {
                self.take(8)?;
                Val::Fixed64
            }
            2 => {
                let n = self.varint()? as usize;
                Val::Bytes(self.take(n)?)
            }
            5 => Val::Fixed32(self.take(4)?.try_into().unwrap()),
            w => bail!("unsupported wire type {w}"),
        };
        Ok(Some((key >> 3, v)))
    }
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn vehicle_position(b: &[u8]) -> Result<Veh> {
    let mut v = Veh::default();
    let mut r = Rd::new(b);
    while let Some((f, val)) = r.next()? {
        match (f, val) {
            (1, Val::Bytes(t)) => {
                let mut r = Rd::new(t);
                while let Some((f, val)) = r.next()? {
                    match (f, val) {
                        (1, Val::Bytes(s)) => v.trip = text(s),
                        (5, Val::Bytes(s)) => v.route = text(s),
                        _ => {}
                    }
                }
            }
            (2, Val::Bytes(p)) => {
                let mut r = Rd::new(p);
                while let Some((f, val)) = r.next()? {
                    match (f, val) {
                        (1, Val::Fixed32(x)) => v.lat = f32::from_le_bytes(x),
                        (2, Val::Fixed32(x)) => v.lon = f32::from_le_bytes(x),
                        _ => {}
                    }
                }
            }
            (5, Val::Varint(t)) => v.ts = t as i64,
            (8, Val::Bytes(d)) => {
                let mut r = Rd::new(d);
                while let Some((f, val)) = r.next()? {
                    if let (1, Val::Bytes(s)) = (f, val) {
                        v.id = text(s);
                    }
                }
            }
            _ => {}
        }
    }
    Ok(v)
}

/// Decode a FeedMessage into its vehicles. Entities without a vehicle id,
/// position or timestamp are skipped.
pub fn decode(b: &[u8]) -> Result<Vec<Veh>> {
    let mut out = Vec::new();
    let mut r = Rd::new(b);
    while let Some((f, val)) = r.next()? {
        if let (2, Val::Bytes(ent)) = (f, val) {
            let mut r = Rd::new(ent);
            while let Some((f, val)) = r.next()? {
                if let (4, Val::Bytes(vp)) = (f, val) {
                    let v = vehicle_position(vp)?;
                    if !v.id.is_empty() && v.ts > 0 && v.lat != 0.0 && v.lon != 0.0 {
                        out.push(v);
                    }
                }
            }
        }
    }
    Ok(out)
}

pub fn read_snapshot(path: &Path) -> Result<Vec<Veh>> {
    let mut raw = Vec::new();
    GzDecoder::new(std::fs::File::open(path)?).read_to_end(&mut raw).with_context(|| format!("reading {}", path.display()))?;
    decode(&raw).with_context(|| format!("decoding {}", path.display()))
}

/// Snapshots for one IST date folder, as (fetch epoch, path), oldest first.
pub fn list_day(raw_dir: &Path, date: &str) -> Result<Vec<(i64, PathBuf)>> {
    let mut v = Vec::new();
    let dir = raw_dir.join(date);
    if !dir.is_dir() {
        return Ok(v);
    }
    for e in std::fs::read_dir(&dir)? {
        let p = e?.path();
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if let Some(stem) = name.strip_suffix(".pb.gz") {
            if let Ok(t) = stem.parse::<i64>() {
                v.push((t, p));
            }
        }
    }
    v.sort();
    Ok(v)
}

/// Date folders present in the archive, sorted.
pub fn list_dates(raw_dir: &Path) -> Result<Vec<String>> {
    let mut v = Vec::new();
    if raw_dir.is_dir() {
        for e in std::fs::read_dir(raw_dir)? {
            let e = e?;
            let n = e.file_name().to_string_lossy().into_owned();
            if e.path().is_dir() && n.len() == 10 && n.as_bytes()[4] == b'-' {
                v.push(n);
            }
        }
    }
    v.sort();
    Ok(v)
}
