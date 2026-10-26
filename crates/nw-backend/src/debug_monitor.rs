//! Extended debug logging's own reports (`logging`), quiet while it is off:
//!
//! * **Resources**, every [`HEARTBEAT`] (debug): the process's working set,
//!   private memory, handles (Windows) or open files (Linux) and threads,
//!   with the system's free memory and commit charge. A backend that grows
//!   before it stops shows here; "RAM looked fine" while the commit limit
//!   ran out shows too.
//! * **Stalls** (warnings): the async runtime not running its timers, or
//!   the shared state ([`AppState`]) unavailable to readers, for
//!   [`STALL`] or more, and how long it lasted. Such a stall is what the
//!   desktop app sees as the backend not answering.
//!
//! It runs on its own thread, so it still reports while the runtime is stuck.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::RwLock;

use crate::app_state::AppState;

/// How often resources are reported.
pub const HEARTBEAT: Duration = Duration::from_secs(30);
/// What counts as a stall.
pub const STALL: Duration = Duration::from_secs(2);
const CHECK: Duration = Duration::from_secs(1);
const RUNTIME_BEAT: Duration = Duration::from_millis(250);
/// While the state stays unavailable, say so again this often.
const STILL_STALLED: Duration = Duration::from_secs(10);

/// Start the reports (from inside the runtime).
pub fn spawn(state: Arc<RwLock<AppState>>) {
    let started = Instant::now();
    let beat = Arc::new(AtomicU64::new(0));
    {
        let beat = Arc::clone(&beat);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(RUNTIME_BEAT);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                beat.store(millis(started.elapsed()), Ordering::Relaxed);
            }
        });
    }
    let spawned = std::thread::Builder::new()
        .name("nw-debug-monitor".into())
        .spawn(move || watch(&state, &beat, started));
    if let Err(error) = spawned {
        tracing::warn!(%error, "Could not start the debug monitor");
    }
}

fn watch(state: &RwLock<AppState>, beat: &AtomicU64, started: Instant) {
    let mut runtime = StallTracker::new(STALL);
    let mut next_heartbeat = Instant::now();
    loop {
        std::thread::sleep(CHECK);
        if !crate::logging::extended() {
            runtime = StallTracker::new(STALL);
            next_heartbeat = Instant::now();
            continue;
        }
        let since_beat = started
            .elapsed()
            .saturating_sub(Duration::from_millis(beat.load(Ordering::Relaxed)));
        match runtime.update(since_beat) {
            Some(StallEvent::Started(quiet)) => tracing::warn!(
                quiet_ms = millis(quiet),
                "The async runtime has stopped running: requests, timers and the cloud link wait"
            ),
            Some(StallEvent::Ended(lasted)) => tracing::warn!(
                lasted_ms = millis(lasted),
                "The async runtime runs again after a stall"
            ),
            None => {}
        }
        let (views, cameras) = read_state(state);
        if Instant::now() >= next_heartbeat {
            next_heartbeat = Instant::now() + HEARTBEAT;
            tracing::debug!(
                resources = %Resources::read().summary(),
                cameras,
                desktop_views = views,
                "Resources"
            );
        }
    }
}

/// Read a little from the state, reporting how long it was unavailable.
fn read_state(state: &RwLock<AppState>) -> (usize, usize) {
    let asked = Instant::now();
    let mut reported: Option<Instant> = None;
    loop {
        if let Ok(st) = state.try_read() {
            let counts = (st.active_views(), st.config.cameras.len());
            drop(st);
            let waited = asked.elapsed();
            if reported.is_some() {
                tracing::warn!(
                    waited_ms = millis(waited),
                    "The shared state is available again"
                );
            }
            return counts;
        }
        let waited = asked.elapsed();
        let due = match reported {
            None => waited >= STALL,
            Some(at) => at.elapsed() >= STILL_STALLED,
        };
        if due && crate::logging::extended() {
            reported = Some(Instant::now());
            tracing::warn!(
                waited_ms = millis(waited),
                "The shared state has been held for a while: requests and camera changes wait"
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// A stall starting or ending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StallEvent {
    Started(Duration),
    Ended(Duration),
}

/// Turns "quiet for this long" readings into one start and one end per stall.
#[derive(Debug, Clone)]
pub struct StallTracker {
    threshold: Duration,
    longest: Option<Duration>,
}

impl StallTracker {
    pub fn new(threshold: Duration) -> Self {
        Self {
            threshold,
            longest: None,
        }
    }

    pub fn update(&mut self, quiet: Duration) -> Option<StallEvent> {
        match self.longest {
            None if quiet >= self.threshold => {
                self.longest = Some(quiet);
                Some(StallEvent::Started(quiet))
            }
            Some(longest) if quiet < self.threshold => {
                self.longest = None;
                Some(StallEvent::Ended(longest))
            }
            Some(longest) => {
                self.longest = Some(longest.max(quiet));
                None
            }
            None => None,
        }
    }
}

/// The process's and the system's resources; what cannot be read is `None`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Resources {
    pub working_set: Option<u64>,
    /// Windows: private commit; Linux: resident anonymous memory.
    pub private: Option<u64>,
    /// Windows: handles; Linux: open file descriptors.
    pub handles: Option<u64>,
    pub threads: Option<u64>,
    pub memory_total: Option<u64>,
    pub memory_available: Option<u64>,
    /// Committed memory and the commit limit (RAM plus page file or swap).
    pub commit: Option<u64>,
    pub commit_limit: Option<u64>,
}

impl Resources {
    pub fn read() -> Self {
        os::read()
    }

    /// One line, e.g. `working set 412 MB, private 380 MB, handles 2311,
    /// threads 143; system memory 9120 of 16304 MB free, commit 12010 of
    /// 20400 MB`.
    pub fn summary(&self) -> String {
        let mb = |bytes: Option<u64>| match bytes {
            Some(bytes) => format!("{} MB", bytes / (1024 * 1024)),
            None => "?".into(),
        };
        let count = |n: Option<u64>| n.map_or_else(|| "?".into(), |n| n.to_string());
        let pair = |a: Option<u64>, b: Option<u64>| match (a, b) {
            (Some(a), Some(b)) => format!("{} of {} MB", a / (1024 * 1024), b / (1024 * 1024)),
            _ => "?".into(),
        };
        format!(
            "working set {}, private {}, handles {}, threads {}; system memory {} free, commit {}",
            mb(self.working_set),
            mb(self.private),
            count(self.handles),
            count(self.threads),
            pair(self.memory_available, self.memory_total),
            pair(self.commit, self.commit_limit),
        )
    }
}

/// `VmRSS`, `RssAnon` and `Threads` of `/proc/self/status` (bytes, bytes,
/// count).
pub fn parse_proc_status(text: &str) -> (Option<u64>, Option<u64>, Option<u64>) {
    let value = |key: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(key))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|n| n.parse::<u64>().ok())
    };
    (
        value("VmRSS:").map(|kib| kib * 1024),
        value("RssAnon:").map(|kib| kib * 1024),
        value("Threads:"),
    )
}

/// `MemTotal`, `MemAvailable`, `Committed_AS` and `CommitLimit` of
/// `/proc/meminfo`, in bytes.
pub fn parse_meminfo(text: &str) -> [Option<u64>; 4] {
    let value = |key: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(key))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|n| n.parse::<u64>().ok())
            .map(|kib| kib * 1024)
    };
    [
        value("MemTotal:"),
        value("MemAvailable:"),
        value("Committed_AS:"),
        value("CommitLimit:"),
    ]
}

#[cfg(not(windows))]
mod os {
    use super::{parse_meminfo, parse_proc_status, Resources};

    pub fn read() -> Resources {
        let (working_set, private, threads) = std::fs::read_to_string("/proc/self/status")
            .map(|text| parse_proc_status(&text))
            .unwrap_or_default();
        let handles = std::fs::read_dir("/proc/self/fd")
            .ok()
            .map(|entries| entries.count() as u64);
        let [memory_total, memory_available, commit, commit_limit] =
            std::fs::read_to_string("/proc/meminfo")
                .map(|text| parse_meminfo(&text))
                .unwrap_or_default();
        Resources {
            working_set,
            private,
            handles,
            threads,
            memory_total,
            memory_available,
            commit,
            commit_limit,
        }
    }
}

#[cfg(windows)]
mod os {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetCurrentProcessId, GetProcessHandleCount,
    };

    use super::Resources;

    pub fn read() -> Resources {
        let mut resources = Resources::default();
        let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        // SAFETY: an all-zero PROCESS_MEMORY_COUNTERS is valid; `cb` tells
        // the call how much it may write.
        let mut counters: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
        counters.cb = size;
        // SAFETY: the pseudo handle needs no closing; `counters` is `size` bytes.
        if unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, size) } != 0 {
            resources.working_set = Some(counters.WorkingSetSize as u64);
            resources.private = Some(counters.PagefileUsage as u64);
        }
        let mut handles: u32 = 0;
        // SAFETY: the pseudo handle needs no closing; `handles` is writable.
        if unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut handles) } != 0 {
            resources.handles = Some(u64::from(handles));
        }
        resources.threads = thread_count();
        // SAFETY: an all-zero MEMORYSTATUSEX is valid; dwLength is set below.
        let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
        status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        // SAFETY: `status` is a valid MEMORYSTATUSEX with its length set.
        if unsafe { GlobalMemoryStatusEx(&mut status) } != 0 {
            resources.memory_total = Some(status.ullTotalPhys);
            resources.memory_available = Some(status.ullAvailPhys);
            // "Page file" here is the commit limit and what is left of it.
            resources.commit_limit = Some(status.ullTotalPageFile);
            resources.commit = Some(
                status
                    .ullTotalPageFile
                    .saturating_sub(status.ullAvailPageFile),
            );
        }
        resources
    }

    fn thread_count() -> Option<u64> {
        // SAFETY: a process snapshot; closed below.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return None;
        }
        // SAFETY: no arguments.
        let me = unsafe { GetCurrentProcessId() };
        // SAFETY: an all-zero PROCESSENTRY32W is valid; dwSize is set below.
        let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut threads = None;
        // SAFETY: `snapshot` is open and `entry` has its size set.
        let mut more = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
        while more {
            if entry.th32ProcessID == me {
                threads = Some(u64::from(entry.cntThreads));
                break;
            }
            // SAFETY: as above.
            more = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
        }
        // SAFETY: opened above, not used after this.
        unsafe { CloseHandle(snapshot) };
        threads
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Duration = Duration::from_secs(1);

    #[test]
    fn a_stall_is_reported_once_when_it_starts_and_once_when_it_ends() {
        let mut tracker = StallTracker::new(2 * S);
        assert_eq!(tracker.update(S), None);
        assert_eq!(tracker.update(3 * S), Some(StallEvent::Started(3 * S)));
        assert_eq!(tracker.update(9 * S), None);
        assert_eq!(tracker.update(4 * S), None);
        assert_eq!(
            tracker.update(Duration::ZERO),
            Some(StallEvent::Ended(9 * S))
        );
        assert_eq!(tracker.update(Duration::ZERO), None);
    }

    #[test]
    fn linux_process_and_memory_figures_are_read() {
        let status = "Name:\tnw-backend\nVmRSS:\t  4096 kB\nRssAnon:\t 1024 kB\nThreads:\t42\n";
        assert_eq!(
            parse_proc_status(status),
            (Some(4096 * 1024), Some(1024 * 1024), Some(42))
        );
        let meminfo = "MemTotal:  16000 kB\nMemFree: 1 kB\nMemAvailable: 8000 kB\n\
                       CommitLimit: 20000 kB\nCommitted_AS: 12000 kB\n";
        assert_eq!(
            parse_meminfo(meminfo),
            [
                Some(16000 * 1024),
                Some(8000 * 1024),
                Some(12000 * 1024),
                Some(20000 * 1024)
            ]
        );
    }

    #[test]
    fn the_summary_says_what_it_could_not_read() {
        let resources = Resources {
            working_set: Some(412 * 1024 * 1024),
            handles: Some(2311),
            memory_total: Some(16 * 1024 * 1024 * 1024),
            memory_available: Some(9 * 1024 * 1024 * 1024),
            ..Resources::default()
        };
        let summary = resources.summary();
        assert!(
            summary.starts_with("working set 412 MB, private ?, handles 2311"),
            "{summary}"
        );
        assert!(
            summary.contains("system memory 9216 of 16384 MB free, commit ?"),
            "{summary}"
        );
    }

    #[test]
    fn this_process_can_be_read() {
        let resources = Resources::read();
        assert!(resources.working_set.is_some() && resources.threads.is_some());
    }
}
