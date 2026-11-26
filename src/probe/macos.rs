//! macOS backend: libproc + mach + sysctl, no subprocesses.
//!
//! Struct layouts come from <sys/proc_info.h>, <sys/resource.h> and
//! <mach/vm_statistics.h>; sizes are checked at runtime against the kernel's
//! return values, so a layout mismatch degrades to "unreadable", never UB.

use super::{Mem, Proc};
use libc::{c_char, c_int, c_void};
use std::ffi::CStr;
use std::sync::OnceLock;

const PROC_ALL_PIDS: u32 = 1;
const PROC_PIDTBSDINFO: c_int = 3;
const PROC_PIDVNODEPATHINFO: c_int = 9;
const PROC_PIDT_SHORTBSDINFO: c_int = 13;
const PROC_PIDPATHINFO_MAXSIZE: usize = 4096;
const RUSAGE_INFO_V4: c_int = 4;
const KERN_PROCARGS2: c_int = 49;
const HOST_VM_INFO64: c_int = 4;
const MAXCOMLEN: usize = 16;
const MAXPATHLEN: usize = 1024;
const ARGS_KEEP: usize = 4096;

#[repr(C)]
struct ProcBsdInfo {
    pbi_flags: u32,
    pbi_status: u32,
    pbi_xstatus: u32,
    pbi_pid: u32,
    pbi_ppid: u32,
    pbi_uid: u32,
    pbi_gid: u32,
    pbi_ruid: u32,
    pbi_rgid: u32,
    pbi_svuid: u32,
    pbi_svgid: u32,
    rfu_1: u32,
    pbi_comm: [c_char; MAXCOMLEN],
    pbi_name: [c_char; 2 * MAXCOMLEN],
    pbi_nfiles: u32,
    pbi_pgid: u32,
    pbi_pjobc: u32,
    e_tdev: u32,
    e_tpgid: u32,
    pbi_nice: i32,
    pbi_start_tvsec: u64,
    pbi_start_tvusec: u64,
}

#[repr(C)]
struct ProcBsdShortInfo {
    pbsi_pid: u32,
    pbsi_ppid: u32,
    pbsi_pgid: u32,
    pbsi_status: u32,
    pbsi_comm: [c_char; MAXCOMLEN],
    pbsi_flags: u32,
    pbsi_uid: u32,
    pbsi_gid: u32,
    pbsi_ruid: u32,
    pbsi_rgid: u32,
    pbsi_svuid: u32,
    pbsi_svgid: u32,
    pbsi_rfu: u32,
}

/// struct vm_statistics64 (HOST_VM_INFO64_COUNT = 38 integers).
#[repr(C)]
#[derive(Default)]
struct VmStats64 {
    free_count: u32,
    active_count: u32,
    inactive_count: u32,
    wire_count: u32,
    zero_fill_count: u64,
    reactivations: u64,
    pageins: u64,
    pageouts: u64,
    faults: u64,
    cow_faults: u64,
    lookups: u64,
    hits: u64,
    purges: u64,
    purgeable_count: u32,
    speculative_count: u32,
    decompressions: u64,
    compressions: u64,
    swapins: u64,
    swapouts: u64,
    compressor_page_count: u32,
    throttled_count: u32,
    external_page_count: u32,
    internal_page_count: u32,
    total_uncompressed_pages_in_compressor: u64,
}

#[repr(C)]
#[derive(Default)]
struct XswUsage {
    xsu_total: u64,
    xsu_avail: u64,
    xsu_used: u64,
    xsu_pagesize: u32,
    xsu_encrypted: u32,
}

#[repr(C)]
#[derive(Default)]
struct MachTimebase {
    numer: u32,
    denom: u32,
}

// vnode_info is 152 bytes; vip_path follows it.
const VNODE_INFO_SIZE: usize = 152;
const VNODE_PATH_INFO_SIZE: usize = 2 * (VNODE_INFO_SIZE + MAXPATHLEN);

extern "C" {
    fn proc_listpids(kind: u32, typeinfo: u32, buffer: *mut c_void, buffersize: c_int) -> c_int;
    fn proc_pidinfo(
        pid: c_int,
        flavor: c_int,
        arg: u64,
        buffer: *mut c_void,
        buffersize: c_int,
    ) -> c_int;
    fn proc_pidpath(pid: c_int, buffer: *mut c_void, buffersize: u32) -> c_int;
    fn proc_pid_rusage(pid: c_int, flavor: c_int, buffer: *mut c_void) -> c_int;
    // Private but stable libSystem symbol; Activity Monitor uses it to attribute
    // helpers (WebKit tabs, VM XPC services, ...) to the app that launched them.
    fn responsibility_get_pid_responsible_for_pid(pid: c_int) -> c_int;
    fn mach_host_self() -> u32;
    fn mach_task_self() -> u32;
    fn mach_port_deallocate(task: u32, name: u32) -> c_int;
    fn host_statistics64(host: u32, flavor: c_int, info: *mut c_int, count: *mut u32) -> c_int;
    fn mach_timebase_info(info: *mut MachTimebase) -> c_int;
    static vm_kernel_page_size: usize;
}

fn timebase() -> (u64, u64) {
    static TB: OnceLock<(u64, u64)> = OnceLock::new();
    *TB.get_or_init(|| {
        let mut tb = MachTimebase::default();
        unsafe { mach_timebase_info(&mut tb) };
        if tb.numer == 0 || tb.denom == 0 {
            (1, 1)
        } else {
            (tb.numer as u64, tb.denom as u64)
        }
    })
}

/// rusage CPU times are mach absolute ticks (not ns) on Apple Silicon.
fn ticks_to_ns(t: u64) -> u64 {
    let (n, d) = timebase();
    ((t as u128) * (n as u128) / (d as u128)) as u64
}

fn cstr(buf: &[c_char]) -> String {
    let bytes: Vec<u8> = buf
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn list_pids() -> Vec<c_int> {
    unsafe {
        let n = proc_listpids(PROC_ALL_PIDS, 0, std::ptr::null_mut(), 0);
        if n <= 0 {
            return Vec::new();
        }
        let cap = n as usize / std::mem::size_of::<c_int>() + 64;
        let mut pids = vec![0 as c_int; cap];
        let bytes = proc_listpids(
            PROC_ALL_PIDS,
            0,
            pids.as_mut_ptr() as *mut c_void,
            (cap * std::mem::size_of::<c_int>()) as c_int,
        );
        if bytes <= 0 {
            return Vec::new();
        }
        pids.truncate(bytes as usize / std::mem::size_of::<c_int>());
        pids.retain(|&p| p > 0);
        pids
    }
}

struct Basic {
    ppid: i32,
    uid: u32,
    start: i64,
    comm: String,
}

fn basic(pid: c_int) -> Option<Basic> {
    unsafe {
        let mut bi: ProcBsdInfo = std::mem::zeroed();
        let sz = std::mem::size_of::<ProcBsdInfo>() as c_int;
        if proc_pidinfo(
            pid,
            PROC_PIDTBSDINFO,
            0,
            &mut bi as *mut _ as *mut c_void,
            sz,
        ) == sz
        {
            return Some(Basic {
                ppid: bi.pbi_ppid as i32,
                uid: bi.pbi_uid,
                start: bi.pbi_start_tvsec as i64,
                comm: cstr(&bi.pbi_comm),
            });
        }
        let mut si: ProcBsdShortInfo = std::mem::zeroed();
        let sz = std::mem::size_of::<ProcBsdShortInfo>() as c_int;
        if proc_pidinfo(
            pid,
            PROC_PIDT_SHORTBSDINFO,
            0,
            &mut si as *mut _ as *mut c_void,
            sz,
        ) == sz
        {
            return Some(Basic {
                ppid: si.pbsi_ppid as i32,
                uid: si.pbsi_uid,
                start: 0,
                comm: cstr(&si.pbsi_comm),
            });
        }
        None
    }
}

/// (footprint bytes, cpu ns) from rusage_info_v4.
fn rusage(pid: c_int) -> Option<(u64, u64)> {
    // rusage_info_v4 is ~300 bytes; a larger buffer is harmless.
    let mut buf = [0u64; 64];
    let rc = unsafe { proc_pid_rusage(pid, RUSAGE_INFO_V4, buf.as_mut_ptr() as *mut c_void) };
    if rc != 0 {
        return None;
    }
    // layout: uuid[16] (2 words), user_time, system_time, pkg_idle_wkups,
    // interrupt_wkups, pageins, wired_size, resident_size, phys_footprint
    let cpu = ticks_to_ns(buf[2]) + ticks_to_ns(buf[3]);
    Some((buf[9], cpu))
}

fn path_of(pid: c_int, buf: &mut [u8]) -> String {
    let n = unsafe { proc_pidpath(pid, buf.as_mut_ptr() as *mut c_void, buf.len() as u32) };
    if n <= 0 {
        return String::new();
    }
    String::from_utf8_lossy(&buf[..n as usize]).into_owned()
}

/// argv joined with spaces, read via KERN_PROCARGS2 into a reusable buffer.
fn args_of(pid: c_int, buf: &mut [u8]) -> String {
    let mut mib = [libc::CTL_KERN, KERN_PROCARGS2, pid];
    let mut size = buf.len();
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buf.as_mut_ptr() as *mut c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || size < 4 {
        return String::new();
    }
    let data = &buf[..size];
    let argc = i32::from_ne_bytes([data[0], data[1], data[2], data[3]]).max(0) as usize;
    let mut p = 4;
    while p < data.len() && data[p] != 0 {
        p += 1; // exec path
    }
    while p < data.len() && data[p] == 0 {
        p += 1; // padding
    }
    let start = p;
    let mut seen = 0;
    while seen < argc && p < data.len() {
        if data[p] == 0 {
            seen += 1;
        }
        p += 1;
    }
    let raw = &data[start..p.min(data.len())];
    let mut s: Vec<u8> = raw.iter().map(|&b| if b == 0 { b' ' } else { b }).collect();
    while s.last() == Some(&b' ') {
        s.pop();
    }
    clip(&String::from_utf8_lossy(&s))
}

/// Keep head and tail of huge command lines: main class and flags live at the ends.
fn clip(s: &str) -> String {
    if s.len() <= ARGS_KEEP {
        return s.to_string();
    }
    let head_end = floor_char(s, ARGS_KEEP * 5 / 8);
    let tail_start = ceil_char(s, s.len() - ARGS_KEEP * 3 / 8);
    format!("{} … {}", &s[..head_end], &s[tail_start..])
}

fn floor_char(s: &str, mut i: usize) -> usize {
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}
fn ceil_char(s: &str, mut i: usize) -> usize {
    while !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

fn argmax() -> usize {
    let mut v: c_int = 0;
    let mut sz = std::mem::size_of::<c_int>();
    let name = c"kern.argmax";
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            &mut v as *mut _ as *mut c_void,
            &mut sz,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc == 0 && v > 0 {
        v as usize
    } else {
        1 << 20
    }
}

/// Every process. Arguments are read only for the current user's processes
/// (the kernel refuses others without root).
pub fn procs() -> Result<Vec<Proc>, String> {
    let pids = list_pids();
    if pids.is_empty() {
        return Err("proc_listpids failed".into());
    }
    let me = unsafe { libc::getuid() };
    let mut argbuf = vec![0u8; argmax()];
    let mut pathbuf = vec![0u8; PROC_PIDPATHINFO_MAXSIZE];
    let mut out = Vec::with_capacity(pids.len());
    for pid in pids {
        let Some(b) = basic(pid) else { continue };
        let mut p = Proc {
            pid,
            ppid: b.ppid,
            uid: b.uid,
            name: b.comm,
            start: b.start,
            resp_pid: unsafe { responsibility_get_pid_responsible_for_pid(pid) },
            ..Default::default()
        };
        if let Some((fp, cpu)) = rusage(pid) {
            p.footprint = fp;
            p.cpu_ns = cpu;
            p.readable = true;
        }
        p.path = path_of(pid, &mut pathbuf);
        if p.uid == me {
            p.args = args_of(pid, &mut argbuf);
        }
        out.push(p);
    }
    Ok(out)
}

fn sysctl_val<T: Default>(name: &CStr) -> Option<T> {
    let mut v = T::default();
    let mut sz = std::mem::size_of::<T>();
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            &mut v as *mut T as *mut c_void,
            &mut sz,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0).then_some(v)
}

/// System-wide memory picture, computed like Activity Monitor.
pub fn memory() -> Result<Mem, String> {
    let mut vs = VmStats64::default();
    let mut count = (std::mem::size_of::<VmStats64>() / std::mem::size_of::<c_int>()) as u32;
    let kr = unsafe {
        let host = mach_host_self();
        let kr = host_statistics64(
            host,
            HOST_VM_INFO64,
            &mut vs as *mut _ as *mut c_int,
            &mut count,
        );
        mach_port_deallocate(mach_task_self(), host); // don't leak a port right per call
        kr
    };
    if kr != 0 {
        return Err("host_statistics64 failed".into());
    }
    let page = unsafe { vm_kernel_page_size } as u64;
    let internal = vs.internal_page_count as u64;
    let purgeable = vs.purgeable_count as u64;
    let swap: XswUsage = sysctl_val(c"vm.swapusage").unwrap_or_default();
    let mut m = Mem {
        total: sysctl_val::<u64>(c"hw.memsize").unwrap_or(0),
        app: internal.saturating_sub(purgeable) * page,
        wired: vs.wire_count as u64 * page,
        compressed: vs.compressor_page_count as u64 * page,
        cached: (vs.external_page_count as u64 + purgeable) * page,
        free: (vs.free_count as u64 + vs.speculative_count as u64) * page,
        swap_total: swap.xsu_total,
        swap_used: swap.xsu_used,
        pressure: sysctl_val::<c_int>(c"kern.memorystatus_vm_pressure_level").unwrap_or(0),
        free_percent: sysctl_val::<c_int>(c"kern.memorystatus_level").unwrap_or(0),
        ..Default::default()
    };
    m.used = m.app + m.wired + m.compressed;
    Ok(m)
}

/// Working directory of a process ("" if unknown).
pub fn cwd(pid: i32) -> String {
    let mut buf = vec![0u8; VNODE_PATH_INFO_SIZE];
    let n = unsafe {
        proc_pidinfo(
            pid,
            PROC_PIDVNODEPATHINFO,
            0,
            buf.as_mut_ptr() as *mut c_void,
            buf.len() as c_int,
        )
    };
    if n as usize != VNODE_PATH_INFO_SIZE {
        return String::new();
    }
    let path = &buf[VNODE_INFO_SIZE..VNODE_INFO_SIZE + MAXPATHLEN];
    let end = path.iter().position(|&b| b == 0).unwrap_or(path.len());
    String::from_utf8_lossy(&path[..end]).into_owned()
}

/// Start time in unix seconds; 0 if alive but unreadable; -1 if gone.
pub fn start_time(pid: i32) -> i64 {
    match basic(pid) {
        Some(b) => b.start,
        None => -1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_match_kernel() {
        assert_eq!(std::mem::size_of::<ProcBsdInfo>(), 136);
        assert_eq!(std::mem::size_of::<ProcBsdShortInfo>(), 64);
        assert_eq!(std::mem::size_of::<VmStats64>() / 4, 38);
    }

    #[test]
    fn reads_own_process() {
        let me = std::process::id() as i32;
        let ps = procs().unwrap();
        assert!(ps.len() > 20);
        let p = ps.iter().find(|p| p.pid == me).expect("own pid listed");
        assert!(p.readable && p.footprint > 0 && p.start > 0);
        assert!(!p.args.is_empty());
        assert_eq!(cwd(me), std::env::current_dir().unwrap().to_string_lossy());
        assert_eq!(start_time(me), p.start);
        assert_eq!(start_time(-5), -1);
        let m = memory().unwrap();
        assert!(m.total > 1 << 30 && m.used > 0 && m.used <= m.total + (1 << 30));
    }
}
