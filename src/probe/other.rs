//! Placeholder for platforms without a backend yet. Linux (/proc + PSI) is next; see PLAN.md.

use super::{Mem, Proc};

const UNSUPPORTED: &str = "ramster: this platform is not supported yet (macOS only for now)";

pub fn procs() -> Result<Vec<Proc>, String> {
    Err(UNSUPPORTED.into())
}
pub fn memory() -> Result<Mem, String> {
    Err(UNSUPPORTED.into())
}
pub fn cwd(_pid: i32) -> String {
    String::new()
}
pub fn start_time(_pid: i32) -> i64 {
    -1
}
