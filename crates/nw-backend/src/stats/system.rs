//! Real backend-process CPU/RAM/network collector.
//!
//! Reads the operating system directly rather than through a full
//! sysinfo/psutil dependency — we need only a handful of well-defined
//! fields: `/proc` on Linux, the Win32 counters on Windows (`os` module).
//! Anything that cannot be read is reported as missing rather than
//! fabricated.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use nw_core::stats::{NetworkThroughput, ProcessBreakdown, ProcessMetrics};

use super::SystemMetricsSource;

/// Collector for the current process + system totals.
pub struct ProcSystemCollector {
    inner: Mutex<Inner>,
}

/// Network rates are measured over at least this long, so snapshots taken
/// close together (several pages polling) do not give jumpy readings.
const MIN_NETWORK_WINDOW: Duration = Duration::from_secs(1);

struct Inner {
    last_proc_ticks: Option<(u64, Instant)>,
    last_system_ticks: Option<(u64, u64)>, // (idle, total)
    last_network: Option<((u64, u64), Instant)>, // (rx, tx) bytes
    last_network_rate: Option<NetworkThroughput>,
}

impl ProcSystemCollector {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                last_proc_ticks: None,
                last_system_ticks: None,
                last_network: None,
                last_network_rate: None,
            }),
        }
    }

    fn read_proc_stat_ticks() -> Option<u64> {
        os::process_cpu_ticks()
    }

    fn read_system_stat_ticks() -> Option<(u64, u64)> {
        os::system_cpu_ticks()
    }

    fn read_rss_bytes() -> Option<u64> {
        os::resident_bytes()
    }

    fn read_meminfo() -> Option<(u64, u64)> {
        os::memory_kib()
    }

    /// Received and sent bytes over the host's real network interfaces.
    fn read_network_bytes() -> Option<(u64, u64)> {
        os::network_bytes()
    }

    fn ticks_per_second() -> u64 {
        os::TICKS_PER_SECOND
    }
}

#[cfg(not(windows))]
mod os {
    use std::fs;

    use super::{parse_meminfo, parse_proc_self_stat, parse_system_stat, parse_vm_rss};

    /// Linux default; sysconf isn't in std.
    pub const TICKS_PER_SECOND: u64 = 100;

    pub fn process_cpu_ticks() -> Option<u64> {
        parse_proc_self_stat(&fs::read_to_string("/proc/self/stat").ok()?)
    }

    pub fn system_cpu_ticks() -> Option<(u64, u64)> {
        parse_system_stat(&fs::read_to_string("/proc/stat").ok()?)
    }

    pub fn resident_bytes() -> Option<u64> {
        parse_vm_rss(&fs::read_to_string("/proc/self/status").ok()?)
    }

    pub fn memory_kib() -> Option<(u64, u64)> {
        parse_meminfo(&fs::read_to_string("/proc/meminfo").ok()?)
    }

    pub fn network_bytes() -> Option<(u64, u64)> {
        let has_device = |name: &str| {
            std::path::Path::new("/sys/class/net")
                .join(name)
                .join("device")
                .exists()
        };
        super::sum_network_bytes(&fs::read_to_string("/proc/net/dev").ok()?, has_device)
    }
}

#[cfg(windows)]
mod os {
    use windows_sys::Win32::Foundation::{FILETIME, NO_ERROR};
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        FreeMibTable, GetIfTable2, IF_TYPE_SOFTWARE_LOOPBACK, MIB_IF_ROW2, MIB_IF_TABLE2,
    };
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetProcessTimes, GetSystemTimes,
    };

    use super::{sum_interfaces, Interface};

    /// Windows reports CPU time in 100 ns units.
    pub const TICKS_PER_SECOND: u64 = 10_000_000;

    /// `MIB_IF_ROW2.InterfaceAndOperStatusFlags` bits.
    const HARDWARE_INTERFACE: u8 = 0x01;
    const FILTER_INTERFACE: u8 = 0x02;

    const fn filetime(time: FILETIME) -> u64 {
        ((time.dwHighDateTime as u64) << 32) | time.dwLowDateTime as u64
    }

    const fn zero_filetime() -> FILETIME {
        FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        }
    }

    pub fn process_cpu_ticks() -> Option<u64> {
        let (mut created, mut exited, mut kernel, mut user) = (
            zero_filetime(),
            zero_filetime(),
            zero_filetime(),
            zero_filetime(),
        );
        // SAFETY: the pseudo handle needs no closing; all four pointers are
        // valid, writable FILETIMEs.
        let ok = unsafe {
            GetProcessTimes(
                GetCurrentProcess(),
                &mut created,
                &mut exited,
                &mut kernel,
                &mut user,
            )
        };
        (ok != 0).then(|| filetime(kernel).saturating_add(filetime(user)))
    }

    /// `(idle, total)`: kernel time already includes idle time.
    pub fn system_cpu_ticks() -> Option<(u64, u64)> {
        let (mut idle, mut kernel, mut user) = (zero_filetime(), zero_filetime(), zero_filetime());
        // SAFETY: all three pointers are valid, writable FILETIMEs.
        let ok = unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) };
        (ok != 0).then(|| {
            (
                filetime(idle),
                filetime(kernel).saturating_add(filetime(user)),
            )
        })
    }

    pub fn resident_bytes() -> Option<u64> {
        let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        // SAFETY: an all-zero PROCESS_MEMORY_COUNTERS is valid; `cb` tells
        // the call how much it may write.
        let mut counters: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
        counters.cb = size;
        // SAFETY: the pseudo handle needs no closing; `counters` is `size` bytes.
        let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, size) };
        (ok != 0).then_some(counters.WorkingSetSize as u64)
    }

    /// `(total, available)` physical memory in KiB, like `/proc/meminfo`.
    pub fn memory_kib() -> Option<(u64, u64)> {
        // SAFETY: an all-zero MEMORYSTATUSEX is valid; dwLength is set below.
        let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
        status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        // SAFETY: `status` is a valid MEMORYSTATUSEX with its length set.
        let ok = unsafe { GlobalMemoryStatusEx(&mut status) };
        (ok != 0).then_some((status.ullTotalPhys / 1024, status.ullAvailPhys / 1024))
    }

    /// Bytes through the network adapters. NDIS filter drivers appear as
    /// extra rows for the same adapter, so those are left out with loopback.
    pub fn network_bytes() -> Option<(u64, u64)> {
        let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
        // SAFETY: on success the call allocates the table, freed below.
        if unsafe { GetIfTable2(&mut table) } != NO_ERROR || table.is_null() {
            return None;
        }
        // SAFETY: GetIfTable2 succeeded, so `table` holds NumEntries rows.
        let rows: &[MIB_IF_ROW2] = unsafe {
            std::slice::from_raw_parts((*table).Table.as_ptr(), (*table).NumEntries as usize)
        };
        let interfaces: Vec<Interface> = rows
            .iter()
            .filter(|row| row.Type != IF_TYPE_SOFTWARE_LOOPBACK)
            .filter(|row| row.InterfaceAndOperStatusFlags._bitfield & FILTER_INTERFACE == 0)
            .map(|row| Interface {
                real: row.InterfaceAndOperStatusFlags._bitfield & HARDWARE_INTERFACE != 0,
                rx: row.InOctets,
                tx: row.OutOctets,
            })
            .collect();
        // SAFETY: allocated by GetIfTable2 and not used after this.
        unsafe { FreeMibTable(table.cast()) };
        sum_interfaces(&interfaces)
    }
}

impl Default for ProcSystemCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl SystemMetricsSource for ProcSystemCollector {
    fn snapshot(&self) -> ProcessMetrics {
        let mut inner = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => {
                return ProcessMetrics::default();
            }
        };

        let now = Instant::now();
        let cpu_percent = match (Self::read_proc_stat_ticks(), inner.last_proc_ticks.take()) {
            (Some(cur), Some((prev, prev_at))) => {
                inner.last_proc_ticks = Some((cur, now));
                process_cpu_percent(cur, prev, now.saturating_duration_since(prev_at))
            }
            (Some(cur), None) => {
                inner.last_proc_ticks = Some((cur, now));
                0.0
            }
            _ => 0.0,
        };

        let system_cpu_percent = match (
            Self::read_system_stat_ticks(),
            inner.last_system_ticks.take(),
        ) {
            (Some((idle, total)), Some(prev)) => {
                inner.last_system_ticks = Some((idle, total));
                system_cpu_percent((idle, total), prev)
            }
            (Some((idle, total)), None) => {
                inner.last_system_ticks = Some((idle, total));
                None
            }
            _ => None,
        };

        let network = match (Self::read_network_bytes(), inner.last_network) {
            (Some(cur), Some((prev, prev_at))) => {
                let elapsed = now.saturating_duration_since(prev_at);
                if elapsed >= MIN_NETWORK_WINDOW {
                    inner.last_network = Some((cur, now));
                    inner.last_network_rate = Some(network_rate(cur, prev, elapsed));
                }
                inner.last_network_rate
            }
            (Some(cur), None) => {
                inner.last_network = Some((cur, now));
                None
            }
            (None, _) => None,
        };

        let rss_bytes = Self::read_rss_bytes().unwrap_or(0);
        let (memory_percent, system_memory_percent) =
            memory_percents(Self::read_meminfo(), rss_bytes);

        ProcessMetrics {
            cpu_percent,
            rss_bytes,
            memory_percent,
            system_cpu_percent,
            system_memory_percent,
            siblings: current_process_breakdown(cpu_percent, rss_bytes),
            network,
        }
    }
}

/// One network interface's byte counters; `real` when backed by a device.
struct Interface {
    real: bool,
    rx: u64,
    tx: u64,
}

/// Total (received, sent) bytes of the real interfaces (Ethernet, Wi-Fi), so
/// traffic through bridges, virtual adapters or VPN tunnels is not counted
/// twice; without any, every interface given (loopback already left out).
fn sum_interfaces(interfaces: &[Interface]) -> Option<(u64, u64)> {
    if interfaces.is_empty() {
        return None;
    }
    let any_real = interfaces.iter().any(|i| i.real);
    Some(
        interfaces
            .iter()
            .filter(|i| i.real || !any_real)
            .fold((0u64, 0u64), |(rx, tx), i| {
                (rx.saturating_add(i.rx), tx.saturating_add(i.tx))
            }),
    )
}

/// Total (received, sent) bytes from `/proc/net/dev` contents, counting the
/// interfaces backed by a device (`has_device`); see `sum_interfaces`.
#[cfg_attr(windows, allow(dead_code))]
fn sum_network_bytes(data: &str, has_device: impl Fn(&str) -> bool) -> Option<(u64, u64)> {
    let interfaces: Vec<Interface> = data
        .lines()
        .filter_map(|line| {
            let (name, counters) = line.split_once(':')?;
            let fields: Vec<u64> = counters
                .split_whitespace()
                .map(str::parse)
                .collect::<Result<_, _>>()
                .ok()?;
            // Receive: bytes packets errs drop fifo frame compressed multicast;
            // then transmit, starting with bytes.
            Some((name.trim(), *fields.first()?, *fields.get(8)?))
        })
        .filter(|(name, _, _)| *name != "lo")
        .map(|(name, rx, tx)| Interface {
            real: has_device(name),
            rx,
            tx,
        })
        .collect();
    sum_interfaces(&interfaces)
}

/// Bits per second between two byte counts. A counter that went back
/// (interface reset, one unplugged) counts as no traffic.
fn network_rate(cur: (u64, u64), prev: (u64, u64), elapsed: Duration) -> NetworkThroughput {
    let secs = elapsed.as_secs_f64();
    let rate = |now: u64, then: u64| -> u64 {
        if secs <= 0.0 {
            return 0;
        }
        (now.saturating_sub(then) as f64 * 8.0 / secs).round() as u64
    };
    NetworkThroughput {
        rx_bits_per_sec: rate(cur.0, prev.0),
        tx_bits_per_sec: rate(cur.1, prev.1),
    }
}

/// Sum of utime + stime (fields 14 and 15) from `/proc/self/stat` contents.
#[cfg_attr(windows, allow(dead_code))]
fn parse_proc_self_stat(data: &str) -> Option<u64> {
    // Fields 14 (utime) and 15 (stime) — but field 2 (comm) may
    // contain spaces & parens. Split on the trailing ')'.
    let (_, rest) = data.split_once(')')?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // After the ')' the state field is index 0, so utime = index 11.
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    Some(utime + stime)
}

/// `(idle, total)` jiffies from the aggregate `cpu` line of `/proc/stat`.
#[cfg_attr(windows, allow(dead_code))]
fn parse_system_stat(data: &str) -> Option<(u64, u64)> {
    let first = data.lines().next()?;
    let mut parts = first.split_whitespace();
    parts.next()?; // "cpu"
    let mut totals: Vec<u64> = Vec::new();
    for p in parts {
        totals.push(p.parse().ok()?);
    }
    if totals.len() < 4 {
        return None;
    }
    let idle = totals[3];
    let total: u64 = totals.iter().sum();
    Some((idle, total))
}

/// `VmRSS` in bytes from `/proc/self/status` contents.
#[cfg_attr(windows, allow(dead_code))]
fn parse_vm_rss(data: &str) -> Option<u64> {
    for line in data.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kb * 1024);
        }
    }
    None
}

/// `(MemTotal, MemAvailable)` in KiB from `/proc/meminfo` contents.
#[cfg_attr(windows, allow(dead_code))]
fn parse_meminfo(data: &str) -> Option<(u64, u64)> {
    let mut total = None;
    let mut available = None;
    for line in data.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            total = rest.split_whitespace().next().and_then(|v| v.parse().ok());
        } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
            available = rest.split_whitespace().next().and_then(|v| v.parse().ok());
        }
    }
    Some((total?, available?))
}

/// Process CPU% from two tick samples; a zero interval counts as one second.
fn process_cpu_percent(cur: u64, prev: u64, elapsed: Duration) -> f32 {
    let elapsed = elapsed.as_secs_f32();
    let elapsed = if elapsed <= 0.0 { 1.0 } else { elapsed };
    let ticks = cur.saturating_sub(prev) as f32;
    (ticks / (ProcSystemCollector::ticks_per_second() as f32) / elapsed) * 100.0
}

/// System busy% from two `(idle, total)` samples; `None` if no time elapsed.
fn system_cpu_percent(cur: (u64, u64), prev: (u64, u64)) -> Option<f32> {
    let d_idle = cur.0.saturating_sub(prev.0) as f32;
    let d_total = cur.1.saturating_sub(prev.1) as f32;
    if d_total <= 0.0 {
        None
    } else {
        Some(((d_total - d_idle) / d_total) * 100.0)
    }
}

/// `(process %, system used %)` of total memory from meminfo KiB and RSS bytes.
fn memory_percents(meminfo: Option<(u64, u64)>, rss_bytes: u64) -> (f32, Option<f32>) {
    match meminfo {
        Some((total_kb, available_kb)) if total_kb > 0 => {
            let used_kb = total_kb.saturating_sub(available_kb);
            let sys_percent = (used_kb as f32) / (total_kb as f32) * 100.0;
            let proc_percent = (rss_bytes as f32) / (total_kb as f32 * 1024.0) * 100.0;
            (proc_percent, Some(sys_percent))
        }
        _ => (0.0, None),
    }
}

fn current_process_breakdown(cpu_percent: f32, rss_bytes: u64) -> Vec<ProcessBreakdown> {
    let pid = std::process::id();
    vec![ProcessBreakdown {
        label: "nw-backend".into(),
        pid,
        cpu_percent,
        rss_bytes,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── /proc/self/stat ──────────────────────────────────────────────

    const SELF_STAT: &str = "1234 (nw-backend) S 1 1234 1234 0 -1 4194560 \
        500 0 0 0 250 75 0 0 20 0 8 0 100 123456789 2048 18446744073709551615";

    const NET_DEV: &str = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo: 9000 10 0 0 0 0 0 0 9000 10 0 0 0 0 0 0
enp3s0: 1000 10 0 0 0 0 0 0 200 5 0 0 0 0 0 0
wlp2s0: 500 4 0 0 0 0 0 0 100 2 0 0 0 0 0 0
docker0: 7000 9 0 0 0 0 0 0 7000 9 0 0 0 0 0 0
";

    #[test]
    fn network_counts_real_interfaces_once() {
        let real = |name: &str| name.starts_with("enp") || name.starts_with("wlp");
        assert_eq!(sum_network_bytes(NET_DEV, real), Some((1500, 300)));
        // Nothing backed by a device (a container): all but loopback.
        assert_eq!(sum_network_bytes(NET_DEV, |_| false), Some((8500, 7300)));
        assert_eq!(sum_network_bytes("Inter-|\n face |\n", real), None);
    }

    #[test]
    fn interfaces_count_real_adapters_or_else_all_given() {
        let row = |real, rx, tx| Interface { real, rx, tx };
        // A virtual adapter (Hyper-V, VPN) beside a real one is left out.
        let mixed = [row(true, 100, 10), row(false, 900, 90), row(true, 1, 1)];
        assert_eq!(sum_interfaces(&mixed), Some((101, 11)));
        let virtual_only = [row(false, 5, 6), row(false, 1, 1)];
        assert_eq!(sum_interfaces(&virtual_only), Some((6, 7)));
        assert_eq!(sum_interfaces(&[]), None);
    }

    #[test]
    fn network_rate_is_bits_per_second_and_survives_counter_resets() {
        let rate = network_rate((3_000, 1_250), (1_000, 1_000), Duration::from_secs(2));
        assert_eq!(rate.rx_bits_per_sec, 8_000);
        assert_eq!(rate.tx_bits_per_sec, 1_000);
        let reset = network_rate((10, 10), (1_000, 1_000), Duration::from_secs(1));
        assert_eq!((reset.rx_bits_per_sec, reset.tx_bits_per_sec), (0, 0));
        assert_eq!(
            network_rate((5, 5), (0, 0), Duration::ZERO).rx_bits_per_sec,
            0
        );
    }

    #[test]
    fn proc_stat_sums_utime_and_stime() {
        assert_eq!(parse_proc_self_stat(SELF_STAT), Some(325));
    }

    #[test]
    fn proc_stat_handles_comm_with_spaces() {
        let data = "42 (tokio runtime w) R 1 42 42 0 -1 0 0 0 0 0 7 3 0 0 20 0 1 0 1 1 1";
        assert_eq!(parse_proc_self_stat(data), Some(10));
    }

    #[test]
    fn proc_stat_rejects_missing_close_paren() {
        assert_eq!(parse_proc_self_stat("1234 (nw-backend S 1 2 3"), None);
    }

    #[test]
    fn proc_stat_rejects_truncated_fields() {
        // stime (index 12 after ')') is missing.
        let data = "1 (x) S 1 1 1 0 -1 0 0 0 0 0 9";
        assert_eq!(parse_proc_self_stat(data), None);
        assert_eq!(parse_proc_self_stat("1 (x) S 1 1"), None);
        assert_eq!(parse_proc_self_stat(""), None);
    }

    #[test]
    fn proc_stat_rejects_non_numeric_ticks() {
        let bad_utime = "1 (x) S 1 1 1 0 -1 0 0 0 0 0 abc 3";
        let bad_stime = "1 (x) S 1 1 1 0 -1 0 0 0 0 0 3 -4";
        assert_eq!(parse_proc_self_stat(bad_utime), None);
        assert_eq!(parse_proc_self_stat(bad_stime), None);
    }

    // ── /proc/stat ───────────────────────────────────────────────────

    #[test]
    fn system_stat_returns_idle_and_total() {
        let data = "cpu  100 20 30 400 50 0 5 0 0 0\ncpu0 50 10 15 200 25 0 2 0 0 0\n";
        assert_eq!(parse_system_stat(data), Some((400, 605)));
    }

    #[test]
    fn system_stat_accepts_minimum_four_fields() {
        assert_eq!(parse_system_stat("cpu 1 2 3 4"), Some((4, 10)));
    }

    #[test]
    fn system_stat_rejects_fewer_than_four_fields() {
        assert_eq!(parse_system_stat("cpu 1 2 3"), None);
        assert_eq!(parse_system_stat("cpu"), None);
    }

    #[test]
    fn system_stat_rejects_empty_and_garbage() {
        assert_eq!(parse_system_stat(""), None);
        assert_eq!(parse_system_stat("cpu 1 2 x 4"), None);
        assert_eq!(parse_system_stat("   \n"), None);
    }

    #[test]
    fn system_stat_only_reads_first_line() {
        // A malformed second line must not affect the aggregate line.
        assert_eq!(parse_system_stat("cpu 1 1 1 1\ngarbage"), Some((1, 4)));
    }

    // ── /proc/self/status ────────────────────────────────────────────

    #[test]
    fn vm_rss_converts_kib_to_bytes() {
        let data = "Name:\tnw-backend\nVmPeak:\t  9000 kB\nVmRSS:\t   2048 kB\nThreads:\t4\n";
        assert_eq!(parse_vm_rss(data), Some(2048 * 1024));
    }

    #[test]
    fn vm_rss_missing_or_malformed_is_none() {
        assert_eq!(parse_vm_rss("Name:\tx\nThreads:\t1\n"), None);
        assert_eq!(parse_vm_rss(""), None);
        assert_eq!(parse_vm_rss("VmRSS:\n"), None);
        assert_eq!(parse_vm_rss("VmRSS:\tlots kB\n"), None);
    }

    // ── /proc/meminfo ────────────────────────────────────────────────

    #[test]
    fn meminfo_returns_total_and_available_in_any_order() {
        let data = "MemTotal:       16000000 kB\nMemFree:  1000 kB\nMemAvailable:   4000000 kB\n";
        assert_eq!(parse_meminfo(data), Some((16_000_000, 4_000_000)));
        let reversed = "MemAvailable: 10 kB\nMemTotal: 20 kB\n";
        assert_eq!(parse_meminfo(reversed), Some((20, 10)));
    }

    #[test]
    fn meminfo_requires_both_fields() {
        assert_eq!(parse_meminfo("MemTotal: 100 kB\n"), None);
        assert_eq!(parse_meminfo("MemAvailable: 100 kB\n"), None);
        assert_eq!(parse_meminfo(""), None);
    }

    #[test]
    fn meminfo_malformed_values_are_none() {
        assert_eq!(
            parse_meminfo("MemTotal: abc kB\nMemAvailable: 1 kB\n"),
            None
        );
        assert_eq!(parse_meminfo("MemTotal: 1 kB\nMemAvailable:\n"), None);
    }

    // ── CPU delta calculations ───────────────────────────────────────

    #[test]
    fn process_cpu_percent_from_tick_delta() {
        // 50 ticks over 1s at 100 Hz = 50%.
        let pct = process_cpu_percent(150, 100, Duration::from_secs(1));
        assert!((pct - 50.0).abs() < 1e-4, "{pct}");
        // 200 ticks over 1s = 200% (multi-core, not clamped).
        let pct = process_cpu_percent(300, 100, Duration::from_secs(1));
        assert!((pct - 200.0).abs() < 1e-4, "{pct}");
        // 100 ticks over 2s = 50%.
        let pct = process_cpu_percent(200, 100, Duration::from_secs(2));
        assert!((pct - 50.0).abs() < 1e-4, "{pct}");
    }

    #[test]
    fn process_cpu_percent_zero_elapsed_uses_one_second() {
        let pct = process_cpu_percent(110, 100, Duration::ZERO);
        assert!((pct - 10.0).abs() < 1e-4, "{pct}");
    }

    #[test]
    fn process_cpu_percent_counter_regression_is_zero() {
        assert_eq!(process_cpu_percent(5, 100, Duration::from_secs(1)), 0.0);
    }

    #[test]
    fn system_cpu_percent_from_deltas() {
        // total +100, idle +25 → 75% busy.
        let pct = system_cpu_percent((125, 1100), (100, 1000)).unwrap_or(-1.0);
        assert!((pct - 75.0).abs() < 1e-4, "{pct}");
        // Fully idle.
        assert_eq!(system_cpu_percent((200, 1100), (100, 1000)), Some(0.0));
        // Fully busy.
        assert_eq!(system_cpu_percent((100, 1100), (100, 1000)), Some(100.0));
    }

    #[test]
    fn system_cpu_percent_no_progress_is_none() {
        assert_eq!(system_cpu_percent((100, 1000), (100, 1000)), None);
        // Counter regression saturates to zero delta → None, never negative.
        assert_eq!(system_cpu_percent((50, 500), (100, 1000)), None);
    }

    // ── memory percentages ───────────────────────────────────────────

    #[test]
    fn memory_percents_compute_process_and_system_share() {
        // 1 GiB total, 256 MiB available, 128 MiB RSS.
        let total_kb = 1024 * 1024;
        let (proc_pct, sys_pct) = memory_percents(Some((total_kb, 256 * 1024)), 128 * 1024 * 1024);
        assert!((proc_pct - 12.5).abs() < 1e-4, "{proc_pct}");
        let sys_pct = sys_pct.unwrap_or(-1.0);
        assert!((sys_pct - 75.0).abs() < 1e-4, "{sys_pct}");
    }

    #[test]
    fn memory_percents_available_exceeding_total_saturates() {
        let (_, sys_pct) = memory_percents(Some((100, 500)), 0);
        assert_eq!(sys_pct, Some(0.0));
    }

    #[test]
    fn memory_percents_unavailable_or_zero_total() {
        assert_eq!(memory_percents(None, 4096), (0.0, None));
        assert_eq!(memory_percents(Some((0, 0)), 4096), (0.0, None));
    }

    // ── collector state machine / snapshot invariants ────────────────

    #[test]
    fn process_breakdown_describes_current_backend() {
        let rows = current_process_breakdown(12.5, 4096);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label, "nw-backend");
        assert_eq!(rows[0].pid, std::process::id());
        assert_eq!(rows[0].cpu_percent, 12.5);
        assert_eq!(rows[0].rss_bytes, 4096);
    }

    #[test]
    fn default_collector_starts_without_baselines() {
        let c = ProcSystemCollector::default();
        let inner = c.inner.lock().unwrap();
        assert!(inner.last_proc_ticks.is_none());
        assert!(inner.last_system_ticks.is_none());
    }

    #[test]
    fn first_snapshot_has_no_cpu_delta() {
        let m = ProcSystemCollector::new().snapshot();
        assert_eq!(m.cpu_percent, 0.0);
        assert!(m.system_cpu_percent.is_none());
    }

    #[test]
    fn snapshots_satisfy_stable_invariants() {
        let c = ProcSystemCollector::new();
        let _ = c.snapshot();
        // Burn a little CPU so the tick counters can move.
        let mut acc = 0u64;
        for i in 0..200_000u64 {
            acc = acc.wrapping_mul(31).wrapping_add(i);
        }
        std::hint::black_box(acc);
        let m = c.snapshot();

        assert!(m.cpu_percent.is_finite() && m.cpu_percent >= 0.0);
        assert!(m.memory_percent.is_finite() && (0.0..=100.0).contains(&m.memory_percent));
        if let Some(p) = m.system_cpu_percent {
            assert!(p.is_finite() && (0.0..=100.0).contains(&p), "{p}");
        }
        if let Some(p) = m.system_memory_percent {
            assert!(p.is_finite() && (0.0..=100.0).contains(&p), "{p}");
        }
        assert_eq!(m.siblings.len(), 1);
        assert_eq!(m.siblings[0].pid, std::process::id());
        assert_eq!(m.siblings[0].rss_bytes, m.rss_bytes);
        assert_eq!(m.siblings[0].cpu_percent, m.cpu_percent);

        if cfg!(target_os = "linux") {
            assert!(m.rss_bytes > 0);
            let inner = c.inner.lock().unwrap();
            assert!(inner.last_proc_ticks.is_some());
            assert!(inner.last_system_ticks.is_some());
        }
    }

    #[test]
    fn poisoned_lock_yields_default_metrics() {
        let c = std::sync::Arc::new(ProcSystemCollector::new());
        let c2 = std::sync::Arc::clone(&c);
        let _ = std::thread::spawn(move || {
            let _guard = c2.inner.lock().unwrap();
            panic!("poison the collector lock");
        })
        .join();
        assert!(c.inner.is_poisoned());
        let m = c.snapshot();
        assert_eq!(m.cpu_percent, 0.0);
        assert_eq!(m.rss_bytes, 0);
        assert!(m.system_cpu_percent.is_none());
        assert!(m.siblings.is_empty());
    }
}
