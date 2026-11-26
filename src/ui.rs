//! Human-readable terminal output.

use std::io::IsTerminal;
use std::sync::OnceLock;

use crate::analyze::report::Report;
use crate::analyze::rules::{Finding, CONFIRM, SAFE};
use crate::analyze::Snapshot;
use crate::probe::pressure_name;
use crate::util::{fmt_gb, fmt_mb, fmt_stamp};

pub const RED: &str = "31";
pub const GREEN: &str = "32";
pub const YELLOW: &str = "33";
pub const BLUE: &str = "34";
pub const CYAN: &str = "36";
pub const DIM: &str = "2";
pub const BOLD: &str = "1";

fn color_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none())
}

pub fn c(code: &str, s: &str) -> String {
    if color_on() {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

pub fn pressure_word(level: i32) -> String {
    let n = pressure_name(level);
    match n {
        "critical" => c(&format!("{RED};{BOLD}"), &n.to_uppercase()),
        "warning" => c(&format!("{YELLOW};{BOLD}"), &n.to_uppercase()),
        _ => c(GREEN, n),
    }
}

fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n - 1).collect::<String>())
    }
}

pub fn mem_line(s: &Snapshot) {
    let m = &s.mem;
    println!(
        "{} {} / {} used · {} compressed · swap {} · pressure {}\n",
        c(BOLD, "Memory"),
        fmt_gb(m.used),
        fmt_gb(m.total),
        fmt_gb(m.compressed),
        fmt_gb(m.swap_used),
        pressure_word(m.pressure)
    );
}

pub fn mem(s: &Snapshot) {
    let m = &s.mem;
    mem_line(s);
    const W: f64 = 48.0;
    let seg = |b: u64| (b as f64 / m.total.max(1) as f64 * W) as usize;
    let (a, w, cp, ca) = (seg(m.app), seg(m.wired), seg(m.compressed), seg(m.cached));
    let rest = (W as usize).saturating_sub(a + w + cp + ca);
    println!(
        "  {}{}{}{}{}",
        c(BLUE, &"█".repeat(a)),
        c(RED, &"█".repeat(w)),
        c(YELLOW, &"█".repeat(cp)),
        c(CYAN, &"▒".repeat(ca)),
        c(DIM, &"░".repeat(rest))
    );
    println!(
        "  {} app {}  {} wired {}  {} compressed {}  {} cache {}  free {}\n",
        c(BLUE, "■"),
        fmt_gb(m.app),
        c(RED, "■"),
        fmt_gb(m.wired),
        c(YELLOW, "■"),
        fmt_gb(m.compressed),
        c(CYAN, "■"),
        fmt_gb(m.cached),
        fmt_gb(m.free)
    );
    if m.compressed > m.total / 4 {
        println!(
            "  {} {} of RAM is compressed memory. That costs CPU and is why the Mac feels slow.\n",
            c(YELLOW, "!"),
            fmt_gb(m.compressed)
        );
    }
}

pub fn groups(s: &Snapshot, n: usize) {
    println!(
        "{}{}",
        c(BOLD, "Top memory users"),
        c(DIM, " (helpers grouped under their app)")
    );
    let total_mb = (s.mem.total >> 20).max(1) as f64;
    for g in s.groups.iter().take(n) {
        let cpu = if g.cpu_percent >= 0.0 {
            format!("{:5.1}% cpu", g.cpu_percent)
        } else {
            String::new()
        };
        let bar = "▇".repeat((g.mb as f64 / total_mb * 60.0) as usize + 1);
        println!(
            "  {:<26} {:>9} {:>4} procs {}  {}",
            trunc(&g.name, 26),
            fmt_mb(g.mb),
            g.procs,
            c(DIM, &cpu),
            c(CYAN, &bar)
        );
    }
    println!();
}

pub fn findings(fs: &[Finding], compact: bool) {
    if fs.is_empty() {
        println!(
            "{}",
            c(
                GREEN,
                "Nothing obvious to free. Memory use looks intentional."
            )
        );
        return;
    }
    let safe: Vec<&Finding> = fs
        .iter()
        .filter(|f| f.safety == SAFE && f.actionable())
        .collect();
    let confirm: Vec<&Finding> = fs
        .iter()
        .filter(|f| f.safety == CONFIRM && f.actionable())
        .collect();
    let info: Vec<&Finding> = fs.iter().filter(|f| !f.actionable()).collect();
    let section = |title: String, hint: &str, list: &[&Finding]| {
        if list.is_empty() {
            return;
        }
        println!("{title}  {}", c(DIM, hint));
        for f in list {
            println!(
                "  {:<9} {}  {}",
                fmt_mb(f.frees_mb),
                f.title,
                c(DIM, &format!("[{}]", f.id))
            );
            if !f.detail.is_empty() {
                println!("            {}", c(DIM, &f.detail));
            }
            if !f.tip.is_empty() && !compact {
                println!("            {} {}", c(CYAN, "tip:"), f.tip);
            }
        }
        println!();
    };
    let sum = |l: &[&Finding]| l.iter().map(|f| f.frees_mb).sum::<u64>();
    section(
        c(
            &format!("{GREEN};{BOLD}"),
            &format!("SAFE TO FREE · {}", fmt_mb(sum(&safe))),
        ),
        "restarts on demand, nothing lost · ramster free --safe",
        &safe,
    );
    section(
        c(
            &format!("{YELLOW};{BOLD}"),
            &format!("NEEDS YOUR OK · {}", fmt_mb(sum(&confirm))),
        ),
        "stops something you may be using · ramster free <id>",
        &confirm,
    );
    if !info.is_empty() {
        println!("{}", c(BOLD, "ADVICE"));
        for f in info {
            let mut line = f.title.clone();
            if !f.detail.is_empty() {
                line += &c(DIM, &format!(" ({})", f.detail));
            }
            println!("  • {line}");
            if !f.tip.is_empty() && !compact {
                println!("    {}", c(DIM, &f.tip));
            }
        }
        println!();
    }
    if compact {
        println!("{}", c(DIM, "Run `ramster suggest` for details and tips, `ramster free --safe` to reclaim the safe items."));
    }
}

pub fn report(r: &Report) {
    println!(
        "{} {} → {}  ({} samples, {:.1} h observed)\n",
        c(BOLD, "Usage report"),
        fmt_stamp(r.from),
        fmt_stamp(r.to),
        r.samples,
        r.covered_hours
    );
    println!(
        "  RAM {:.0} GB · avg used {:.1} GB · avg compressed {:.1} GB (peak {:.1}) · peak swap {:.1} GB",
        r.total_gb, r.avg_used_gb, r.avg_compressed_gb, r.peak_compressed_gb, r.peak_swap_gb
    );
    println!(
        "  Pressure: {} normal  {} warning  {} critical\n",
        c(GREEN, &format!("{:.0}%", r.pressure_normal_percent)),
        c(YELLOW, &format!("{:.0}%", r.pressure_warning_percent)),
        c(RED, &format!("{:.0}%", r.pressure_critical_percent))
    );
    if r.pressure_by_hour_percent.iter().any(|&v| v > 0.0) {
        let blocks: Vec<char> = " ▁▂▃▄▅▆▇█".chars().collect();
        let line: String = r
            .pressure_by_hour_percent
            .iter()
            .map(|v| blocks[((v / 100.0) * (blocks.len() - 1) as f64) as usize])
            .collect();
        println!(
            "  Pressure by hour  {}\n                    {}\n",
            c(YELLOW, &line),
            c(DIM, "0     6     12    18   23")
        );
    }
    println!(
        "{}{}",
        c(BOLD, "Heaviest apps"),
        c(
            DIM,
            "  (avg while running · share of time running · idle share)"
        )
    );
    for u in &r.apps {
        println!(
            "  {:<26} {:>9}  peak {:<9}  on {:>3.0}%  idle {:>3.0}%  cpu {:>4.1}%",
            trunc(&u.name, 26),
            fmt_mb(u.avg_mb),
            fmt_mb(u.peak_mb),
            u.present_percent,
            u.idle_percent,
            u.avg_cpu_percent
        );
    }
    println!();
    if !r.forgotten.is_empty() {
        println!(
            "{}{}",
            c(BOLD, "Forgotten: memory held while doing nothing"),
            c(DIM, "  (GB × hours idle)")
        );
        for u in &r.forgotten {
            println!(
                "  {:<26} {:>6.1} GB·h  {}",
                trunc(&u.name, 26),
                u.idle_gb_hours,
                c(
                    DIM,
                    &format!(
                        "{} avg, idle {:.0}% of its time",
                        fmt_mb(u.avg_mb),
                        u.idle_percent
                    )
                )
            );
        }
        println!();
    }
    if !r.advice.is_empty() {
        println!("{}", c(BOLD, "What to change"));
        for (i, a) in r.advice.iter().enumerate() {
            println!("  {}. {a}", i + 1);
        }
        println!();
    }
}
