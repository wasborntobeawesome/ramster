//! Raw process and memory data from the OS. Each platform provides
//! `procs`, `memory`, `cwd`, `start_time` and `parent_of`.

use serde::Serialize;

/// One process at a single instant.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Proc {
    pub pid: i32,
    pub ppid: i32,
    pub uid: u32,
    pub name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub path: String,
    /// Command line, middle-truncated when huge (java classpaths).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub args: String,
    /// Unix seconds, 0 if unknown.
    pub start: i64,
    /// Bytes; same metric as Activity Monitor's "Memory" (includes compressed).
    pub footprint: u64,
    /// User + system CPU time in nanoseconds.
    pub cpu_ns: u64,
    /// The "responsible" process: the app that owns this helper.
    pub resp_pid: i32,
    /// False when the footprint could not be read (another user's process).
    pub readable: bool,
    /// Desktop app the process belongs to when the OS says so directly
    /// (Linux: its systemd scope). macOS groups by .app bundle and leaves it empty.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub app: String,
}

/// System-wide memory. All sizes in bytes.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Mem {
    pub total: u64,
    /// app + wired + compressed (Activity Monitor "Memory Used").
    pub used: u64,
    pub app: u64,
    pub wired: u64,
    /// Physical memory occupied by the compressor.
    pub compressed: u64,
    /// File cache, reclaimable.
    pub cached: u64,
    pub free: u64,
    pub swap_total: u64,
    pub swap_used: u64,
    /// 1 normal, 2 warning, 4 critical.
    pub pressure: i32,
    pub free_percent: i32,
}

pub fn pressure_name(level: i32) -> &'static str {
    match level {
        l if l >= 4 => "critical",
        l if l >= 2 => "warning",
        1 => "normal",
        _ => "unknown",
    }
}

const ARGS_KEEP: usize = 4096;

/// Keep head and tail of huge command lines: main class and flags live at the ends.
pub(crate) fn clip(s: &str) -> String {
    if s.len() <= ARGS_KEEP {
        return s.to_string();
    }
    let mut head_end = ARGS_KEEP * 5 / 8;
    while !s.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = s.len() - ARGS_KEEP * 3 / 8;
    while !s.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    format!("{} … {}", &s[..head_end], &s[tail_start..])
}

#[cfg(any(target_os = "linux", test))]
pub mod procfs;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::{cwd, memory, parent_of, procs, start_time};

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "linux")]
pub use linux::{cwd, memory, parent_of, procs, start_time};

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod other;
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub use other::{cwd, memory, parent_of, procs, start_time};
