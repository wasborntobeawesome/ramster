//! Compact periodic samples written by the daemon (one NDJSON file per day),
//! and "how long has X been idle?" queries over them.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::util::{local_date, now_unix, run};

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

/// `YYYY-MM-DD` of a history file name (`.ndjson` or `.ndjson.gz`).
fn day_of(name: &str) -> Option<&str> {
    let date = name
        .strip_suffix(".ndjson.gz")
        .or_else(|| name.strip_suffix(".ndjson"))?;
    (date.len() == 10).then_some(date)
}

/// The system gzip; we shell out rather than link a compression crate.
fn gzip() -> &'static str {
    if Path::new("/usr/bin/gzip").is_file() {
        "/usr/bin/gzip"
    } else {
        "gzip"
    }
}

/// Parse the samples newer than `since` from one day file into `out`.
fn read_samples(r: impl BufRead, since: i64, out: &mut Vec<Sample>) {
    for line in r.lines().map_while(Result::ok) {
        if let Ok(s) = serde_json::from_str::<Sample>(&line) {
            if s.t >= since {
                out.push(s);
            }
        }
    }
}

/// Reads only the day files that can contain samples after `since`. Cost is
/// bounded by the files on disk, however far back `since` is. Finished days may
/// be gzipped (`.ndjson.gz`, streamed through `gzip -dc`); a day present in both
/// forms is read from both and deduplicated by timestamp.
pub fn load_from(dir: &Path, since: i64) -> Vec<Sample> {
    // One day of slack for time-zone/DST edges; names are YYYY-MM-DD so they sort.
    let first_day = local_date(since.saturating_sub(86_400).max(0));
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            (day_of(&name)? >= first_day.as_str()).then(|| e.path())
        })
        .collect();
    files.sort();
    let mut out = Vec::new();
    for f in files {
        if f.extension().is_some_and(|x| x == "gz") {
            let Ok(mut child) = Command::new(gzip())
                .arg("-dc")
                .arg(&f)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
            else {
                continue;
            };
            if let Some(stdout) = child.stdout.take() {
                read_samples(BufReader::new(stdout), since, &mut out);
            }
            let _ = child.wait();
        } else if let Ok(file) = std::fs::File::open(&f) {
            read_samples(BufReader::new(file), since, &mut out);
        }
    }
    out.sort_by_key(|s| s.t);
    out.dedup_by_key(|s| s.t);
    out
}

/// The daemon's periodic housekeeping: gzip finished days, then delete days
/// older than the retention window.
pub fn prune(retention_days: u32) {
    let now = now_unix();
    compress_in(&dir(), now);
    prune_in(&dir(), retention_days, now);
}

/// Gzip day files from before today that haven't been written to for 10
/// minutes (about 3 MB a day shrinks ~10x). A day that also has a `.gz` (a late
/// write after compression, or an interrupted gzip) is merged into one file first.
pub fn compress_in(dir: &Path, now: i64) {
    let today = local_date(now);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(date) = name.strip_suffix(".ndjson") else {
            continue;
        };
        if date.len() != 10 || date >= today.as_str() {
            continue;
        }
        let settled = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| m.elapsed().ok())
            .is_some_and(|age| age > Duration::from_secs(600));
        if !settled {
            continue;
        }
        let path = e.path();
        let gz = dir.join(format!("{name}.gz"));
        if gz.exists() && !merge_gz_into(&gz, &path) {
            continue;
        }
        let (ok, out) = run(
            Duration::from_secs(120),
            gzip(),
            &["-f", &path.to_string_lossy()],
        );
        if !ok {
            eprintln!("gzip {}: {out}", path.display());
            // gzip removes the plain file only on success; a partial .gz must
            // not stay next to it.
            if path.exists() {
                let _ = std::fs::remove_file(&gz);
            }
        }
    }
}

/// A day in both forms (a late write after compression, or an interrupted
/// gzip): append what the `.gz` holds to the plain file and drop the `.gz`, so
/// the day can be compressed again as one file. Duplicates are fine: loading
/// dedups by timestamp. A truncated `.gz` contributes what is readable.
fn merge_gz_into(gz: &Path, plain: &Path) -> bool {
    let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(plain) else {
        return false;
    };
    // Start on a fresh line in case the plain file ends mid-line.
    if f.write_all(b"\n").is_err() {
        return false;
    }
    let Ok(out) = f.try_clone() else {
        return false;
    };
    let ran = Command::new(gzip())
        .arg("-dc")
        .arg(gz)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::null())
        .status();
    // A corrupt archive still yields its readable prefix; only a failure to
    // run gzip at all leaves the files alone.
    if ran.is_err() || f.write_all(b"\n").is_err() {
        return false;
    }
    std::fs::remove_file(gz).is_ok()
}

/// Delete day files (plain or gzipped) older than the retention window.
pub fn prune_in(dir: &Path, retention_days: u32, now: i64) {
    let cutoff = local_date(now - retention_days as i64 * 86_400);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if day_of(&name).is_some_and(|date| date < cutoff.as_str()) {
            let _ = std::fs::remove_file(e.path());
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

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ramster-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Append samples at times `ts` to `dir/<name>` and backdate its mtime by `age`.
    fn write_day(dir: &Path, name: &str, ts: &[i64], age: u64) -> PathBuf {
        let path = dir.join(name);
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        for &t in ts {
            let s = Sample {
                t,
                ..Default::default()
            };
            writeln!(f, "{}", serde_json::to_string(&s).unwrap()).unwrap();
        }
        let mtime = std::time::SystemTime::now() - Duration::from_secs(age);
        f.set_modified(mtime).unwrap();
        path
    }

    fn times(samples: &[Sample]) -> Vec<i64> {
        samples.iter().map(|s| s.t).collect()
    }

    #[test]
    fn gz_day_file_loads() {
        let dir = temp_dir("hist-gz");
        let t = now_unix() - 2 * 86_400;
        let path = write_day(&dir, &format!("{}.ndjson", local_date(t)), &[t, t + 60], 0);
        assert!(
            run(
                Duration::from_secs(10),
                gzip(),
                &["-f", &path.to_string_lossy()]
            )
            .0
        );
        assert!(!path.exists());
        assert_eq!(times(&load_from(&dir, t - 10)), vec![t, t + 60]);
        assert_eq!(times(&load_from(&dir, t + 30)), vec![t + 60]);
        // The date filter still skips gz days that can't contain newer samples.
        assert!(load_from(&dir, now_unix()).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn compress_old_days_only() {
        let dir = temp_dir("hist-compress");
        let now = now_unix();
        let (d3, d2, d1) = (now - 3 * 86_400, now - 2 * 86_400, now - 86_400);
        let day = |t: i64| format!("{}.ndjson", local_date(t));
        let old = write_day(&dir, &day(d3), &[d3], 3600);
        // Finished day, but written to moments ago: wait for it to settle.
        let fresh = write_day(&dir, &day(d1), &[d1], 0);
        let today = write_day(&dir, &day(now), &[now], 3600);
        // A day in both forms (late write after compression): merged, then compressed.
        let both = write_day(&dir, &day(d2), &[d2, d2 + 60], 3600);
        assert!(
            run(
                Duration::from_secs(10),
                gzip(),
                &["-f", &both.to_string_lossy()]
            )
            .0
        );
        write_day(&dir, &day(d2), &[d2 + 60, d2 + 120], 3600);

        compress_in(&dir, now);

        assert!(!old.exists());
        assert!(dir.join(format!("{}.gz", day(d3))).exists());
        assert!(fresh.exists() && today.exists() && !both.exists());
        assert!(dir.join(format!("{}.gz", day(d2))).exists());
        assert!(!dir.join(format!("{}.gz", day(d1))).exists());
        assert!(!dir.join(format!("{}.gz", day(now))).exists());
        assert_eq!(
            times(&load_from(&dir, d3 - 10)),
            vec![d3, d2, d2 + 60, d2 + 120, d1, now]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn truncated_gz_is_merged_not_stuck() {
        let dir = temp_dir("hist-truncated");
        let now = now_unix();
        let d = now - 2 * 86_400;
        let name = format!("{}.ndjson", local_date(d));
        let many: Vec<i64> = (0..400).map(|i| d + i * 60).collect();
        let plain = write_day(&dir, &name, &many, 3600);
        assert!(
            run(
                Duration::from_secs(10),
                gzip(),
                &["-f", &plain.to_string_lossy()]
            )
            .0
        );
        // Interrupted gzip: half an archive next to a plain file.
        let gz = dir.join(format!("{name}.gz"));
        let bytes = std::fs::read(&gz).unwrap();
        std::fs::write(&gz, &bytes[..bytes.len() / 2]).unwrap();
        write_day(&dir, &name, &[d + 400 * 60], 3600);

        compress_in(&dir, now);

        assert!(!plain.exists() && gz.exists(), "one compressed file again");
        let got = times(&load_from(&dir, d - 10));
        assert!(
            got.contains(&(d + 400 * 60)),
            "the plain file's samples survive"
        );
        // Whatever part of the broken archive was readable is kept too (here maybe none).
        assert!(!got.is_empty() && got.len() <= 401, "{}", got.len());
        // And the next pass leaves the day alone.
        compress_in(&dir, now);
        assert!(!plain.exists() && gz.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn prune_removes_old_plain_and_gz() {
        let dir = temp_dir("hist-prune");
        let now = now_unix();
        let old = now - 10 * 86_400;
        let recent = now - 86_400;
        let a = write_day(&dir, &format!("{}.ndjson", local_date(old)), &[old], 0);
        let b = write_day(
            &dir,
            &format!("{}.ndjson.gz", local_date(old - 86_400)),
            &[],
            0,
        );
        let c = write_day(&dir, &format!("{}.ndjson.gz", local_date(recent)), &[], 0);
        let d = write_day(&dir, &format!("{}.ndjson", local_date(now)), &[now], 0);
        std::fs::write(dir.join("notes.txt"), "kept").unwrap();
        prune_in(&dir, 7, now);
        assert!(!a.exists() && !b.exists());
        assert!(c.exists() && d.exists() && dir.join("notes.txt").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
