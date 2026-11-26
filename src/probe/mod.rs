//! Raw process and memory data from the OS. Each platform provides
//! `procs`, `memory`, `cwd` and `start_time`.

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

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::{cwd, memory, procs, start_time};

#[cfg(not(target_os = "macos"))]
mod other;
#[cfg(not(target_os = "macos"))]
pub use other::{cwd, memory, procs, start_time};
