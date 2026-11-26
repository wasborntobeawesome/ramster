//! Small helpers: time, formatting, subprocesses. Kept dependency-free on purpose.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn uid() -> u32 {
    unsafe { libc::getuid() }
}

/// Local broken-down time (year, month 1-12, day, hour, minute).
pub fn local(t: i64) -> (i32, u32, u32, u32, u32) {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let tt = t as libc::time_t;
    unsafe { libc::localtime_r(&tt, &mut tm) };
    (
        tm.tm_year + 1900,
        tm.tm_mon as u32 + 1,
        tm.tm_mday as u32,
        tm.tm_hour as u32,
        tm.tm_min as u32,
    )
}

pub fn local_date(t: i64) -> String {
    let (y, m, d, _, _) = local(t);
    format!("{y:04}-{m:02}-{d:02}")
}

pub fn local_hour(t: i64) -> usize {
    local(t).3 as usize
}

pub fn fmt_stamp(t: i64) -> String {
    const MON: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let (_, m, d, h, mi) = local(t);
    format!("{} {} {:02}:{:02}", MON[(m as usize - 1) % 12], d, h, mi)
}

pub fn fmt_mb(mb: u64) -> String {
    if mb >= 1024 {
        format!("{:.1} GB", mb as f64 / 1024.0)
    } else {
        format!("{mb} MB")
    }
}

pub fn fmt_gb(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / (1u64 << 30) as f64)
}

pub fn fmt_minutes(m: f64) -> String {
    let mi = m as u64;
    if m < 1.0 {
        "<1m".into()
    } else if mi < 60 {
        format!("{mi}m")
    } else if mi < 24 * 60 {
        format!("{}h{:02}m", mi / 60, mi % 60)
    } else {
        format!("{}d{}h", mi / 1440, mi % 1440 / 60)
    }
}

pub fn home() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/tmp".into())
}

pub fn tildify(p: &str) -> String {
    let h = home();
    match p.strip_prefix(&h) {
        Some(rest) if !h.is_empty() => format!("~{rest}"),
        _ => p.to_string(),
    }
}

pub fn base_name(s: &str) -> &str {
    s.trim_end_matches('/').rsplit('/').next().unwrap_or(s)
}

pub fn slug(s: &str) -> String {
    let mut out = String::new();
    for ch in s.to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_string()
}

/// Run a command with a timeout; returns (success, combined trimmed output).
pub fn run(timeout: Duration, cmd: &str, args: &[&str]) -> (bool, String) {
    let child = Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let Ok(mut child) = child else {
        return (false, format!("cannot run {cmd}"));
    };
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return (false, format!("{cmd} timed out"));
            }
        }
    }
    match child.wait_with_output() {
        Ok(o) => {
            let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
            s.push_str(&String::from_utf8_lossy(&o.stderr));
            (o.status.success(), s.trim().to_string())
        }
        Err(e) => (false, e.to_string()),
    }
}

pub fn which(name: &str, fallbacks: &[&str]) -> Option<String> {
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':') {
            let p = format!("{dir}/{name}");
            if std::path::Path::new(&p).is_file() {
                return Some(p);
            }
        }
    }
    fallbacks
        .iter()
        .find(|p| std::path::Path::new(p).is_file())
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn formatting() {
        assert_eq!(slug("Google Chrome"), "google-chrome");
        assert_eq!(slug("firebase-tools mcp"), "firebase-tools-mcp");
        assert_eq!(fmt_mb(512), "512 MB");
        assert_eq!(fmt_mb(2048), "2.0 GB");
        assert_eq!(fmt_minutes(130.0), "2h10m");
        assert_eq!(fmt_minutes(2580.0), "1d19h");
        assert_eq!(base_name("/opt/homebrew/bin/firebase"), "firebase");
    }
}
