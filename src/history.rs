//! Compact periodic samples written by the daemon (one NDJSON file per day),
//! and "how long has X been idle?" queries over them.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

use crate::util::{local_date, now_unix};

/// One daemon tick. Sizes in MB; CPU in permille of one core (1000 = one full core)
/// averaged since the previous tick.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Sample {
    pub t: i64,
    pub s: Sys,
    #[serde(default)]
    pub g: Vec<Group>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub p: Vec<ProcRec>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Sys {
    pub tot: u32,
    pub used: u32,
    pub comp: u32,
    pub wired: u32,
    pub swap: u32,
    pub pr: i32,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Group {
    pub n: String,
    pub m: u32,
    pub c: u32,
    pub k: u16,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ProcRec {
    pub i: i32,
    pub st: i64,
    pub m: u32,
    pub c: u32,
}

pub fn dir() -> PathBuf {
    crate::config::home().join("history")
}

fn file_for(t: i64) -> PathBuf {
    dir().join(format!("{}.ndjson", local_date(t)))
}

pub fn append(s: &Sample) -> std::io::Result<()> {
    std::fs::create_dir_all(dir())?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file_for(s.t))?;
    let mut line = serde_json::to_vec(s).map_err(std::io::Error::other)?;
    line.push(b'\n');
    f.write_all(&line)
}

/// Samples newer than `since` (unix seconds), oldest first.
pub fn load(since: i64) -> Vec<Sample> {
    load_from(&dir(), since)
}

/// Reads only the day files that can contain samples after `since`. Cost is
/// bounded by the files on disk, however far back `since` is.
pub fn load_from(dir: &std::path::Path, since: i64) -> Vec<Sample> {
    // One day of slack for time-zone/DST edges; names are YYYY-MM-DD so they sort.
    let first_day = local_date(since.saturating_sub(86_400).max(0));
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let date = name.strip_suffix(".ndjson")?.to_string();
            (date.len() == 10 && date >= first_day).then(|| e.path())
        })
        .collect();
    files.sort();
    let mut out = Vec::new();
    for f in files {
        let Ok(file) = std::fs::File::open(&f) else {
            continue;
        };
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            if let Ok(s) = serde_json::from_str::<Sample>(&line) {
                if s.t >= since {
                    out.push(s);
                }
            }
        }
    }
    out.sort_by_key(|s| s.t);
    out.dedup_by_key(|s| s.t);
    out
}

/// Delete day files older than the retention window.
pub fn prune(retention_days: u32) {
    let cutoff = local_date(now_unix() - retention_days as i64 * 86_400);
    let Ok(entries) = std::fs::read_dir(dir()) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if let Some(date) = name.strip_suffix(".ndjson") {
            if date < cutoff.as_str() {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

/// How busy something was over a recent window.
#[derive(Clone, Copy, Debug, Default)]
pub struct Activity {
    /// Minutes observed within the window.
    pub minutes: f64,
    /// Peak CPU permille over the window.
    pub max_cpu: u32,
}

/// A process across samples: PIDs get reused, start times don't.
pub type Key = (i32, i64);

/// Per-process and per-group activity over the trailing window.
pub fn recent(
    samples: &[Sample],
    window_secs: i64,
) -> (HashMap<Key, Activity>, HashMap<String, Activity>) {
    let mut procs: HashMap<Key, Activity> = HashMap::new();
    let mut groups: HashMap<String, Activity> = HashMap::new();
    let cutoff = now_unix() - window_secs;
    let mut prev_t = 0i64;
    for s in samples {
        if s.t < cutoff {
            prev_t = s.t;
            continue;
        }
        let mut dt = 1.0;
        if prev_t > 0 {
            dt = (s.t - prev_t) as f64 / 60.0;
            if !(0.0..=5.0).contains(&dt) {
                dt = 1.0; // daemon wasn't running; don't credit the gap
            }
        }
        prev_t = s.t;
        for p in &s.p {
            let a = procs.entry((p.i, p.st)).or_default();
            a.minutes += dt;
            a.max_cpu = a.max_cpu.max(p.c);
        }
        for g in &s.g {
            let a = groups.entry(g.n.clone()).or_default();
            a.minutes += dt;
            a.max_cpu = a.max_cpu.max(g.c);
        }
    }
    (procs, groups)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_is_bounded_by_files_on_disk() {
        let dir = std::env::temp_dir().join(format!("ramster-hist-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let now = now_unix();
        let old = now - 3 * 86_400;
        for t in [old, now - 60, now] {
            let line = serde_json::to_string(&Sample {
                t,
                ..Default::default()
            })
            .unwrap();
            let path = dir.join(format!("{}.ndjson", local_date(t)));
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .unwrap();
            writeln!(f, "{line}").unwrap();
        }
        std::fs::write(dir.join("notes.txt"), "ignored").unwrap();
        let t0 = std::time::Instant::now();
        let all = load_from(&dir, i64::MIN / 2); // absurd window must not loop
        assert!(t0.elapsed() < std::time::Duration::from_secs(1));
        assert_eq!(all.len(), 3);
        assert_eq!(load_from(&dir, now - 120).len(), 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
