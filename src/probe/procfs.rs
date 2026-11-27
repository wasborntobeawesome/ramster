//! Pure parsers for Linux /proc and /sys text. Kept free of I/O so they are
//! unit-tested on every platform; `linux.rs` does the reading.

use std::collections::{HashMap, HashSet};

use super::Mem;

/// The fields of /proc/<pid>/stat that ramster uses.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Stat {
    pub comm: String,
    pub ppid: i32,
    pub flags: u64,
    /// Clock ticks.
    pub utime: u64,
    pub stime: u64,
    /// Clock ticks since boot.
    pub starttime: u64,
}

/// Kernel threads have no memory or command line of their own.
pub const PF_KTHREAD: u64 = 0x0020_0000;

/// `pid (comm) state ppid ...`; comm may contain spaces and parentheses.
pub fn parse_stat(s: &str) -> Option<Stat> {
    let open = s.find('(')?;
    let close = s.rfind(')')?;
    let comm = s.get(open + 1..close)?.to_string();
    let f: Vec<&str> = s.get(close + 1..)?.split_whitespace().collect();
    // f[0] is field 3 (state); field n is f[n - 3].
    let num = |n: usize| f.get(n - 3)?.parse::<u64>().ok();
    Some(Stat {
        comm,
        ppid: f.get(1)?.parse().ok()?,
        flags: num(9)?,
        utime: num(14)?,
        stime: num(15)?,
        starttime: num(22)?,
    })
}

/// Real UID from /proc/<pid>/status.
pub fn parse_status_uid(s: &str) -> Option<u32> {
    s.lines()
        .find_map(|l| l.strip_prefix("Uid:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// Bytes this process is responsible for: proportional resident set plus its
/// proportional share of swap (the closest match to macOS's phys_footprint).
pub fn parse_smaps_rollup(s: &str) -> u64 {
    let kb = |key: &str| {
        s.lines()
            .find_map(|l| l.strip_prefix(key))
            .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
            .unwrap_or(0)
    };
    (kb("Pss:") + kb("SwapPss:")) << 10
}

/// "KEY: value kB" lines → key → kB.
pub fn parse_meminfo(s: &str) -> HashMap<&str, u64> {
    s.lines()
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            Some((k.trim(), v.split_whitespace().next()?.parse().ok()?))
        })
        .collect()
}

/// PSI averages from /proc/pressure/memory (percent of time stalled).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Psi {
    pub some10: f64,
    pub some60: f64,
    pub full10: f64,
    pub full60: f64,
}

pub fn parse_psi(s: &str) -> Option<Psi> {
    let mut p = Psi::default();
    let mut seen = false;
    for line in s.lines() {
        let Some(kind) = line.split_whitespace().next() else {
            continue;
        };
        let get = |key: &str| {
            line.split_whitespace()
                .find_map(|t| t.strip_prefix(key))
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(0.0)
        };
        match kind {
            "some" => (p.some10, p.some60) = (get("avg10="), get("avg60=")),
            "full" => (p.full10, p.full60) = (get("avg10="), get("avg60=")),
            _ => continue,
        }
        seen = true;
    }
    seen.then_some(p)
}

/// Map stalls to macOS-style levels: 1 normal, 2 warning, 4 critical.
/// Without PSI (old kernels, some containers), fall back to available memory.
pub fn pressure_level(psi: Option<Psi>, available_percent: i32) -> i32 {
    match psi {
        Some(p) => {
            if p.full10 >= 10.0 || p.full60 >= 5.0 || p.some60 >= 40.0 {
                4
            } else if p.some10 >= 10.0 || p.some60 >= 5.0 || p.full10 >= 1.0 {
                2
            } else {
                1
            }
        }
        None if available_percent < 5 => 4,
        None if available_percent < 15 => 2,
        None => 1,
    }
}

/// System memory from /proc/meminfo, PSI and the bytes zram uses.
pub fn mem_from(meminfo: &str, psi: Option<&str>, zram_bytes: u64) -> Mem {
    let m = parse_meminfo(meminfo);
    let kb = |k: &str| m.get(k).copied().unwrap_or(0) << 10;
    let total = kb("MemTotal");
    let available = if m.contains_key("MemAvailable") {
        kb("MemAvailable")
    } else {
        kb("MemFree") + kb("Cached") + kb("Buffers")
    };
    let used = total.saturating_sub(available);
    // zswap's pool, or zram devices: RAM holding compressed pages.
    let compressed = kb("Zswap") + zram_bytes;
    let wired = kb("SUnreclaim") + kb("KernelStack") + kb("PageTables");
    let cached = (kb("Buffers") + kb("Cached") + kb("SReclaimable")).saturating_sub(kb("Shmem"));
    let free_percent = (available * 100).checked_div(total).unwrap_or(0) as i32;
    let swap_total = kb("SwapTotal");
    Mem {
        total,
        used,
        app: used.saturating_sub(wired + compressed),
        wired,
        compressed,
        cached,
        free: kb("MemFree"),
        swap_total,
        swap_used: swap_total.saturating_sub(kb("SwapFree")),
        pressure: pressure_level(psi.and_then(parse_psi), free_percent),
        free_percent,
    }
}

/// `mem_used_total` (third field) of /sys/block/zram*/mm_stat.
pub fn parse_zram_mm_stat(s: &str) -> u64 {
    s.split_whitespace()
        .nth(2)
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// `btime` (boot time, unix seconds) from /proc/stat.
pub fn parse_btime(s: &str) -> Option<i64> {
    s.lines()
        .find_map(|l| l.strip_prefix("btime "))?
        .trim()
        .parse()
        .ok()
}

/// The systemd unit a process runs in, from the cgroup v2 line of
/// /proc/<pid>/cgroup ("0::/user.slice/.../app-firefox-1234.scope").
pub fn unit_of_cgroup(s: &str) -> Option<&str> {
    let path = s.lines().find_map(|l| l.strip_prefix("0::"))?;
    let leaf = path.rsplit('/').next()?;
    (leaf.ends_with(".scope") || leaf.ends_with(".service")).then_some(leaf)
}

/// Started on purpose by a service manager (systemd services, D-Bus activation),
/// not by a person from a terminal or launcher.
pub fn is_managed_unit(unit: &str) -> bool {
    unit.ends_with(".service") && !unit.starts_with("app-") && !unit.starts_with("user@")
}

/// systemd escapes '-' inside names as `\x2d`.
fn unescape(s: &str) -> String {
    s.replace("\\x2d", "-")
}

/// The desktop app a unit belongs to, by the names desktop launchers use:
/// `app-gnome-firefox-1234.scope`, `app-flatpak-org.mozilla.firefox-1234.scope`,
/// `app-org.kde.konsole@3f2a.service`, `snap.firefox.firefox-<uuid>.scope`.
/// Terminal tabs (`vte-spawn-…`), logins (`session-3.scope`) and plain services
/// are not apps.
pub fn app_of_unit(unit: &str) -> Option<String> {
    let base = unit
        .strip_suffix(".scope")
        .or_else(|| unit.strip_suffix(".service"))?;
    if let Some(rest) = base.strip_prefix("snap.") {
        return Some(display_name(rest.split('.').next()?));
    }
    let mut id = unescape(base.strip_prefix("app-")?);
    // app-<launcher>-<id>-<instance>: drop the instance, then the launcher.
    if let Some(i) = id.find('@') {
        id.truncate(i);
    } else if let Some((head, tail)) = id.rsplit_once('-') {
        if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_hexdigit()) {
            id = head.to_string();
        }
    }
    for launcher in [
        "gnome-",
        "kde-",
        "flatpak-",
        "xfce-",
        "cinnamon-",
        "mate-",
        "sway-",
        "niri-",
        "Hyprland-",
    ] {
        if let Some(rest) = id.strip_prefix(launcher) {
            if !rest.is_empty() {
                id = rest.to_string();
                break;
            }
        }
    }
    if id.starts_with("dbus-") || id.is_empty() {
        return None;
    }
    Some(display_name(&id))
}

/// Friendly names for common Linux app ids, matching the macOS app names the
/// rules know ("Google Chrome", "Visual Studio Code").
pub fn display_name(id: &str) -> String {
    const NAMES: &[(&str, &str)] = &[
        ("firefox", "Firefox"),
        ("org.mozilla.firefox", "Firefox"),
        ("google-chrome", "Google Chrome"),
        ("com.google.Chrome", "Google Chrome"),
        ("chromium", "Chromium"),
        ("chromium-browser", "Chromium"),
        ("org.chromium.Chromium", "Chromium"),
        ("brave-browser", "Brave Browser"),
        ("com.brave.Browser", "Brave Browser"),
        ("microsoft-edge", "Microsoft Edge"),
        ("com.microsoft.Edge", "Microsoft Edge"),
        ("code", "Visual Studio Code"),
        ("com.visualstudio.code", "Visual Studio Code"),
        ("cursor", "Cursor"),
        ("windsurf", "Windsurf"),
        ("slack", "Slack"),
        ("com.slack.Slack", "Slack"),
        ("discord", "Discord"),
        ("com.discordapp.Discord", "Discord"),
        ("spotify", "Spotify"),
        ("com.spotify.Client", "Spotify"),
        ("org.telegram.desktop", "Telegram"),
        ("telegram-desktop", "Telegram"),
        ("thunderbird", "Thunderbird"),
        ("org.mozilla.Thunderbird", "Thunderbird"),
        ("jetbrains-idea", "IntelliJ IDEA"),
        ("jetbrains-idea-ce", "IntelliJ IDEA CE"),
        ("jetbrains-pycharm", "PyCharm"),
        ("jetbrains-webstorm", "WebStorm"),
        ("jetbrains-goland", "GoLand"),
        ("jetbrains-clion", "CLion"),
        ("jetbrains-rider", "Rider"),
        ("jetbrains-rustrover", "RustRover"),
        ("jetbrains-studio", "Android Studio"),
        ("android-studio", "Android Studio"),
        ("obsidian", "Obsidian"),
        ("md.obsidian.Obsidian", "Obsidian"),
        ("org.gnome.Nautilus", "Files"),
    ];
    if let Some((_, n)) = NAMES.iter().find(|(k, _)| k.eq_ignore_ascii_case(id)) {
        return n.to_string();
    }
    // Reverse-DNS ids: "org.kde.konsole" → "konsole".
    if id.matches('.').count() >= 2 {
        return id.rsplit('.').next().unwrap_or(id).to_string();
    }
    id.to_string()
}

/// Terminal emulators: what runs inside them keeps its own name.
pub fn is_terminal_app(app: &str) -> bool {
    const T: &[&str] = &[
        "konsole",
        "Terminal",
        "gnome-terminal",
        "kitty",
        "alacritty",
        "Alacritty",
        "wezterm",
        "org.wezfurlong.wezterm",
        "foot",
        "tilix",
        "ptyxis",
        "Ptyxis",
        "ghostty",
        "com.mitchellh.ghostty",
        "xterm",
        "terminator",
        "warp",
        "rio",
        "tabby",
    ];
    T.iter().any(|t| t.eq_ignore_ascii_case(app))
}

/// Socket inodes in LISTEN state from /proc/net/tcp or /proc/net/tcp6.
pub fn parse_listen_inodes(s: &str) -> HashSet<u64> {
    s.lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            // sl local rem st tx:rx tr:tm retrnsmt uid timeout inode
            (f.get(3) == Some(&"0A")).then(|| f.get(9)?.parse().ok())?
        })
        .collect()
}

/// "socket:[12345]" → 12345 (a /proc/<pid>/fd link target).
pub fn socket_inode(link: &str) -> Option<u64> {
    link.strip_prefix("socket:[")?
        .strip_suffix(']')?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_with_odd_comm() {
        let s = "4242 (Web Content (x)) S 4100 4242 4242 0 -1 4194560 5000 0 12 0 731 202 0 0 20 0 30 0 987654 3000000000 50000 18446744073709551615";
        let st = parse_stat(s).unwrap();
        assert_eq!(st.comm, "Web Content (x)");
        assert_eq!(
            (st.ppid, st.utime, st.stime, st.starttime),
            (4100, 731, 202, 987654)
        );
        assert_eq!(st.flags & PF_KTHREAD, 0);
        let kthread = "2 (kthreadd) S 0 0 0 0 -1 2129984 0 0 0 0 0 0 0 0 20 0 1 0 0 0 0";
        assert_ne!(parse_stat(kthread).unwrap().flags & PF_KTHREAD, 0);
        assert_eq!(parse_stat("garbage"), None);
    }

    #[test]
    fn status_and_smaps() {
        assert_eq!(
            parse_status_uid("Name:\tbash\nUid:\t1000\t1000\t1000\t1000\n"),
            Some(1000)
        );
        let smaps = "55d0-7ffc ---p 00000000 00:00 0 [rollup]\nRss:  204800 kB\nPss:  150000 kB\nPss_Anon: 120000 kB\nSwap: 4096 kB\nSwapPss:  2048 kB\n";
        assert_eq!(parse_smaps_rollup(smaps), (150000 + 2048) << 10);
        assert_eq!(parse_smaps_rollup(""), 0);
    }

    const MEMINFO: &str = "MemTotal:       16000000 kB
MemFree:         1000000 kB
MemAvailable:    6000000 kB
Buffers:          200000 kB
Cached:          5000000 kB
SwapCached:        10000 kB
Shmem:            400000 kB
SReclaimable:     300000 kB
SUnreclaim:       200000 kB
KernelStack:       30000 kB
PageTables:        70000 kB
SwapTotal:       8000000 kB
SwapFree:        7000000 kB
Zswap:            500000 kB
Zswapped:        1500000 kB
";

    #[test]
    fn memory_from_meminfo() {
        let psi = "some avg10=0.00 avg60=0.10 avg300=0.00 total=1\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n";
        let m = mem_from(MEMINFO, Some(psi), 100 << 20);
        let kb = |v: u64| v << 10;
        assert_eq!(m.total, kb(16_000_000));
        assert_eq!(m.used, kb(10_000_000));
        assert_eq!(m.compressed, kb(500_000) + (100 << 20));
        assert_eq!(m.wired, kb(300_000));
        assert_eq!(m.cached, kb(5_100_000));
        assert_eq!(m.swap_used, kb(1_000_000));
        assert_eq!((m.free_percent, m.pressure), (37, 1));
    }

    #[test]
    fn pressure_levels() {
        let psi = |s: &str| parse_psi(s);
        assert_eq!(
            pressure_level(psi("some avg10=12.0 avg60=3.0 avg300=1 total=9\nfull avg10=0.5 avg60=0.1 avg300=0 total=1"), 30),
            2
        );
        assert_eq!(
            pressure_level(psi("some avg10=60 avg60=45 avg300=20 total=9\nfull avg10=20 avg60=8 avg300=3 total=9"), 30),
            4
        );
        assert_eq!(pressure_level(None, 3), 4);
        assert_eq!(pressure_level(None, 10), 2);
        assert_eq!(pressure_level(None, 40), 1);
    }

    #[test]
    fn units_and_apps() {
        let cg = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-gnome-firefox-12345.scope\n";
        assert_eq!(unit_of_cgroup(cg), Some("app-gnome-firefox-12345.scope"));
        assert_eq!(unit_of_cgroup("0::/\n"), None);
        let app = |u: &str| app_of_unit(u);
        assert_eq!(
            app("app-gnome-firefox-12345.scope").as_deref(),
            Some("Firefox")
        );
        assert_eq!(
            app("app-flatpak-org.mozilla.firefox-4242.scope").as_deref(),
            Some("Firefox")
        );
        assert_eq!(
            app("app-org.kde.konsole@3f2a9b.service").as_deref(),
            Some("konsole")
        );
        assert_eq!(
            app("app-gnome-google\\x2dchrome-999.scope").as_deref(),
            Some("Google Chrome")
        );
        assert_eq!(
            app("app-code-1a2b3c.scope").as_deref(),
            Some("Visual Studio Code")
        );
        assert_eq!(
            app("snap.slack.slack-8d1e6c.scope").as_deref(),
            Some("Slack")
        );
        assert_eq!(app("vte-spawn-0b1c2d.scope"), None);
        assert_eq!(app("session-3.scope"), None);
        assert_eq!(app("pipewire.service"), None);
        assert_eq!(
            app("app-dbus\\x2d:1.2\\x2dorg.freedesktop.portal.Desktop.scope"),
            None
        );
        assert!(is_managed_unit("pipewire.service"));
        assert!(is_managed_unit(
            "dbus-:1.2-org.gnome.Shell.Notifications@0.service"
        ));
        assert!(!is_managed_unit("app-org.kde.konsole@3f2a9b.service"));
        assert!(!is_managed_unit("app-gnome-firefox-12345.scope"));
        assert!(is_terminal_app("konsole") && !is_terminal_app("Firefox"));
    }

    #[test]
    fn misc_parsers() {
        assert_eq!(
            parse_btime("cpu  1 2 3\nbtime 1764000000\nprocesses 9\n"),
            Some(1_764_000_000)
        );
        assert_eq!(
            parse_zram_mm_stat("  1048576   262144   300000  0  400000  0 0 0 0\n"),
            300_000
        );
        let tcp = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 55555 1 0000 100 0 0 10 0
   1: 0100007F:A3C2 0100007F:1F90 01 00000000:00000000 00:00000000 00000000  1000        0 66666 1 0000 20 4 30 10 -1
";
        assert_eq!(parse_listen_inodes(tcp), HashSet::from([55555]));
        assert_eq!(socket_inode("socket:[55555]"), Some(55555));
        assert_eq!(socket_inode("/dev/null"), None);
    }
}
