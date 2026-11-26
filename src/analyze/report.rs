//! Behaviour report over the daemon's history.

use serde::Serialize;
use std::collections::HashMap;

use crate::history::Sample;
use crate::util::{fmt_mb, local_hour};

#[derive(Clone, Debug, Default, Serialize)]
pub struct AppUsage {
    pub name: String,
    /// Average while it was running.
    pub avg_mb: u64,
    pub peak_mb: u64,
    /// Share of the window it was running.
    pub present_percent: f64,
    pub avg_cpu_percent: f64,
    /// Share of its running time spent idle (<0.5% CPU).
    pub idle_percent: f64,
    /// Memory held while idle, GB × hours.
    pub idle_gb_hours: f64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Report {
    pub from: i64,
    pub to: i64,
    pub samples: usize,
    pub covered_hours: f64,
    pub total_gb: f64,
    pub avg_used_gb: f64,
    pub avg_compressed_gb: f64,
    pub peak_compressed_gb: f64,
    pub peak_swap_gb: f64,
    pub pressure_normal_percent: f64,
    pub pressure_warning_percent: f64,
    pub pressure_critical_percent: f64,
    /// % of samples at warning or worse, per local hour.
    pub pressure_by_hour_percent: [f64; 24],
    /// By average memory.
    pub apps: Vec<AppUsage>,
    /// By idle GB·h.
    pub forgotten: Vec<AppUsage>,
    pub advice: Vec<String>,
}

#[derive(Default)]
struct Acc {
    mb_min: f64,
    cpu_min: f64,
    present: f64,
    idle: f64,
    idle_gbh: f64,
    peak: u64,
}

pub fn build(samples: &[Sample]) -> Report {
    let mut r = Report::default();
    let (Some(first), Some(last)) = (samples.first(), samples.last()) else {
        return r;
    };
    r.from = first.t;
    r.to = last.t;
    r.samples = samples.len();

    let mut apps: HashMap<&str, Acc> = HashMap::new();
    let (mut total, mut used, mut comp, mut warn, mut crit) = (0.0, 0.0, 0.0, 0.0, 0.0);
    let mut hour_n = [0f64; 24];
    let mut hour_warn = [0f64; 24];
    let mut prev_t = 0i64;
    for s in samples {
        let mut dt = 1.0;
        if prev_t > 0 {
            dt = (s.t - prev_t) as f64 / 60.0;
            if !(0.0..=5.0).contains(&dt) || dt == 0.0 {
                dt = 1.0;
            }
        }
        prev_t = s.t;
        total += dt;
        used += s.s.used as f64 * dt;
        comp += s.s.comp as f64 * dt;
        r.total_gb = s.s.tot as f64 / 1024.0;
        r.peak_compressed_gb = r.peak_compressed_gb.max(s.s.comp as f64 / 1024.0);
        r.peak_swap_gb = r.peak_swap_gb.max(s.s.swap as f64 / 1024.0);
        let h = local_hour(s.t);
        hour_n[h] += 1.0;
        if s.s.pr >= 4 {
            crit += dt;
            hour_warn[h] += 1.0;
        } else if s.s.pr >= 2 {
            warn += dt;
            hour_warn[h] += 1.0;
        }
        for g in &s.g {
            let a = apps.entry(g.n.as_str()).or_default();
            a.present += dt;
            a.mb_min += g.m as f64 * dt;
            a.cpu_min += g.c as f64 / 10.0 * dt;
            a.peak = a.peak.max(g.m as u64);
            if g.c < 5 {
                a.idle += dt;
                a.idle_gbh += g.m as f64 / 1024.0 * dt / 60.0;
            }
        }
    }
    r.covered_hours = total / 60.0;
    r.avg_used_gb = used / total / 1024.0;
    r.avg_compressed_gb = comp / total / 1024.0;
    r.pressure_warning_percent = warn / total * 100.0;
    r.pressure_critical_percent = crit / total * 100.0;
    r.pressure_normal_percent = 100.0 - r.pressure_warning_percent - r.pressure_critical_percent;
    for h in 0..24 {
        if hour_n[h] > 0.0 {
            r.pressure_by_hour_percent[h] = hour_warn[h] / hour_n[h] * 100.0;
        }
    }
    for (name, a) in apps {
        let u = AppUsage {
            name: name.to_string(),
            avg_mb: (a.mb_min / a.present) as u64,
            peak_mb: a.peak,
            present_percent: a.present / total * 100.0,
            avg_cpu_percent: a.cpu_min / a.present,
            idle_percent: a.idle / a.present * 100.0,
            idle_gb_hours: a.idle_gbh,
        };
        if u.avg_mb >= 100 {
            r.apps.push(u);
        }
    }
    r.apps
        .sort_by(|a, b| b.avg_mb.cmp(&a.avg_mb).then(a.name.cmp(&b.name)));
    r.forgotten = r
        .apps
        .iter()
        .filter(|u| u.idle_gb_hours >= 0.5 && u.idle_percent >= 50.0 && u.avg_mb >= 500)
        .cloned()
        .collect();
    r.forgotten
        .sort_by(|a, b| b.idle_gb_hours.total_cmp(&a.idle_gb_hours));
    r.apps.truncate(15);
    r.advice = advise(&r);
    r
}

fn app_advice(name: &str, avg: &str) -> Option<String> {
    Some(match name {
        "Docker" => format!("Docker averaged {avg}. Turn on Resource Saver and cap its memory (Settings → Resources), or run `docker desktop stop` when you aren't using containers."),
        "Android emulator" => format!("The Android emulator averaged {avg}. Close it when you aren't testing, or use a physical device over USB/Wi-Fi."),
        "Gradle daemons" => format!("Gradle daemons averaged {avg}. Set org.gradle.daemon.idletimeout=1800000 and let `ramster daemon` with auto_free_safe stop idle ones."),
        "Kotlin daemon" => format!("The Kotlin daemon averaged {avg}. It goes away with the Gradle daemons; auto_free_safe handles both."),
        "AI agents" => format!("Agent sessions averaged {avg}. Archive finished sessions, and enable heavy MCP servers only in the projects that use them."),
        "iOS Simulator" => format!("Simulators averaged {avg}. Run `xcrun simctl shutdown all` when you're done."),
        "Google Chrome" => format!("Chrome averaged {avg}. Turn on Memory Saver (chrome://settings/performance)."),
        "Safari" => format!("Safari averaged {avg}. Close tabs you're not using; each tab is its own process."),
        _ => return None,
    })
}

fn advise(r: &Report) -> Vec<String> {
    let mut out = Vec::new();
    let elevated = r.pressure_warning_percent + r.pressure_critical_percent;
    if elevated >= 10.0 {
        out.push(format!(
            "Memory pressure was elevated {elevated:.0}% of the time: macOS was compressing and swapping instead of running your work. That is the slowdown you feel."
        ));
    }
    let mut seen: Vec<&str> = Vec::new();
    for u in r.forgotten.iter().take(6) {
        seen.push(&u.name);
        let idle = format!(
            " It sat idle {:.0}% of the time ({:.1} GB·h wasted).",
            u.idle_percent, u.idle_gb_hours
        );
        match app_advice(&u.name, &fmt_mb(u.avg_mb)) {
            Some(a) => out.push(a + &idle),
            None => {
                out.push(format!(
                "{} held {} while idle {:.0}% of the time ({:.1} GB·h). Quit it when you're done.",
                u.name, fmt_mb(u.avg_mb), u.idle_percent, u.idle_gb_hours
            ))
            }
        }
    }
    for u in &r.apps {
        if out.len() >= 8 {
            break;
        }
        if u.avg_mb >= 2048 && !seen.contains(&u.name.as_str()) {
            if let Some(a) = app_advice(&u.name, &fmt_mb(u.avg_mb)) {
                out.push(a);
            }
        }
    }
    out
}
