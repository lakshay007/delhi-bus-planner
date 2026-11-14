//! Archive the GTFS-Realtime VehiclePositions feed while the server runs
//! (`busplan serve --collect`), so one process does everything.
//!
//! Every distinct response is stored untouched, gzipped, as
//! `<raw_dir>/<YYYY-MM-DD in Delhi time>/<epoch seconds>.pb.gz`: the layout
//! the live view and `busplan extract` read. A response identical to the
//! previous one is not stored again, so there is one file per feed refresh.

use crate::clock::{Local, now_epoch};
use anyhow::{Context, Result, bail};
use flate2::Compression;
use flate2::write::GzEncoder;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Seconds between polls. The feed refreshes about every 24 s.
pub const INTERVAL_S: u64 = 10;

fn fetch(agent: &ureq::Agent, url: &str, key: &str) -> Result<(u16, Vec<u8>)> {
    let mut res = agent.get(url).header("x-api-key", key).header("User-Agent", "gtfsrt-archiver").call()?;
    let status = res.status().as_u16();
    let body = res.body_mut().with_config().limit(64 * 1024 * 1024).read_to_vec()?;
    Ok((status, body))
}

fn store(raw_dir: &Path, now: i64, body: &[u8]) -> Result<()> {
    let day = raw_dir.join(Local::from_epoch(now).date_string());
    std::fs::create_dir_all(&day)?;
    let tmp = day.join(format!(".{now}.tmp"));
    let mut gz = GzEncoder::new(std::fs::File::create(&tmp)?, Compression::new(6));
    gz.write_all(body)?;
    gz.finish()?;
    std::fs::rename(&tmp, day.join(format!("{now}.pb.gz"))).context("storing a feed snapshot")
}

/// Poll the feed forever. Errors are reported once per change and retried
/// with a growing pause, so a network outage or a sleeping laptop is survived.
pub fn run(raw_dir: PathBuf, url: String, key: String) {
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).timeout_global(Some(Duration::from_secs(20))).build().into();
    let (mut last_hash, mut last_msg, mut backoff, mut stored) = (0u64, String::new(), 0u64, 0usize);
    loop {
        let now = now_epoch();
        let outcome = fetch(&agent, &url, &key).and_then(|(status, body)| {
            if status != 200 || body.len() < 16 {
                bail!("the feed answered HTTP {status} with {} bytes{}", body.len(), if status == 401 || status == 403 { "; check TRANSPORTSTACK_API_KEY" } else { "" });
            }
            let mut h = std::collections::hash_map::DefaultHasher::new();
            body.hash(&mut h);
            let h = h.finish();
            if h != last_hash {
                store(&raw_dir, now, &body)?;
                last_hash = h;
                stored += 1;
            }
            Ok(())
        });
        let msg = match outcome {
            Ok(()) => {
                backoff = 0;
                if stored == 1 && last_msg != "ok" {
                    eprintln!("feed: first snapshot stored in {}", raw_dir.display());
                }
                "ok".to_string()
            }
            Err(e) => {
                backoff = (backoff * 2).clamp(10, 300);
                format!("{e:#}")
            }
        };
        if msg != last_msg && msg != "ok" {
            eprintln!("feed: {msg}; retrying");
        } else if msg == "ok" && last_msg != "ok" && !last_msg.is_empty() {
            eprintln!("feed: reachable again");
        }
        last_msg = msg;
        std::thread::sleep(Duration::from_secs(INTERVAL_S + backoff));
    }
}
