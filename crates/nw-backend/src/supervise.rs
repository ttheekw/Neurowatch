//! Keep the backend running where no service manager restarts it.
//!
//! systemd restarts the backend's unit when it fails (`Restart=on-failure`),
//! but a backend started as a detached background process (Windows, and
//! Linux without a systemd user manager, which many ARM boards run) had
//! nothing watching it: a crash stopped recording until someone noticed.
//! The service manager (`ui/neurowatch/service/manager.py`) therefore starts
//! such a backend as `nw-backend --supervise`: this small parent process
//! runs the real backend as its child and starts it again when it stops
//! unexpectedly.
//!
//! * Exit status 0: the backend stopped on request (`system.shutdown`, a
//!   signal); the supervisor exits too.
//! * [`RESTART_EXIT_CODE`]: the backend asked to be started again (a changed
//!   video decoder, `runtime::restart_in_place`); it starts at once.
//! * Anything else (a crash, a kill): logged with what the status means
//!   ([`describe_exit`], e.g. `0xC0000005, access violation`), then started
//!   again after a growing pause ([`Policy`]). The new backend learns why
//!   ([`previous_exit`]) and says so in its log and its "started" event.
//!
//! A first run that fails within [`STARTUP_GRACE`] (a port in use, a broken
//! install) ends the supervisor with the same status, so whoever started
//! the service sees a failed start instead of a restart loop.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, Instant};

use tokio::sync::watch;

/// The argument that makes `nw-backend` the supervisor.
pub const FLAG: &str = "--supervise";
/// Set on the supervised backend (`"1"`).
const SUPERVISED_ENV: &str = "NW_SUPERVISED";
/// How many times the supervisor started the backend again.
const RESTARTS_ENV: &str = "NW_RESTARTS";
/// Why the previous backend stopped, when it stopped unexpectedly.
const PREVIOUS_EXIT_ENV: &str = "NW_PREVIOUS_EXIT";
/// The supervisor's process id (`system.status` `supervisor_pid`): a
/// service manager stopping the backend waits for both to exit.
const SUPERVISOR_PID_ENV: &str = "NW_SUPERVISOR_PID";
/// Exit status asking to be started again (`runtime::restart_in_place`).
pub const RESTART_EXIT_CODE: i32 = 75;
/// A first run that fails sooner than this is a failed start, not a crash.
pub const STARTUP_GRACE: Duration = Duration::from_secs(10);
/// Pauses before starting a crashed backend again, growing with each crash.
const BACKOFF: [Duration; 4] = [
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
    Duration::from_secs(30),
];
/// A backend that ran this long before crashing starts the pauses over.
const STABLE_AFTER: Duration = Duration::from_secs(10 * 60);

/// `nw-backend --supervise`, and not the supervised backend itself.
pub fn requested() -> bool {
    !supervised() && std::env::args_os().skip(1).any(|arg| arg == FLAG)
}

/// This backend runs under the supervisor.
pub fn supervised() -> bool {
    std::env::var(SUPERVISED_ENV).is_ok_and(|value| value == "1")
}

/// The supervisor's process id, when this backend runs under one.
pub fn supervisor_pid() -> Option<u32> {
    if !supervised() {
        return None;
    }
    std::env::var(SUPERVISOR_PID_ENV).ok()?.parse().ok()
}

/// Why the previous backend stopped unexpectedly, and how many times it was
/// started again, when the supervisor started this one after a crash.
pub fn previous_exit() -> Option<(String, u32)> {
    if !supervised() {
        return None;
    }
    let reason = std::env::var(PREVIOUS_EXIT_ENV)
        .ok()
        .filter(|r| !r.is_empty())?;
    let restarts = std::env::var(RESTARTS_ENV)
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(1);
    Some((reason, restarts))
}

/// Run the supervisor until the backend stops on request; returns the exit
/// status for the process.
pub fn main() -> i32 {
    // Its own small log, into the same service log as the backend's.
    let _ = tracing_subscriber::fmt()
        .pretty()
        .with_env_filter(tracing_subscriber::EnvFilter::new("nw_backend=info"))
        .try_init();
    let program = match std::env::current_exe() {
        Ok(program) => program,
        Err(error) => {
            tracing::error!(%error, "Could not find the backend program to supervise");
            return 1;
        }
    };
    let args: Vec<OsString> = std::env::args_os()
        .skip(1)
        .filter(|arg| arg != FLAG)
        .collect();
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::error!(%error, "Could not start the supervisor");
            return 1;
        }
    };
    runtime.block_on(async move {
        let (stop_tx, stop_rx) = watch::channel(false);
        tokio::spawn(async move {
            stop_signal().await;
            let _ = stop_tx.send(true);
        });
        tracing::info!(
            pid = std::process::id(),
            "Supervising the backend: it is started again if it stops unexpectedly"
        );
        supervise(&Child { program, args }, Policy::default(), stop_rx).await
    })
}

/// The backend program the supervisor runs.
pub struct Child {
    pub program: PathBuf,
    pub args: Vec<OsString>,
}

/// How a run of the backend ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Exit status 0: stopped on request.
    Stopped,
    /// [`RESTART_EXIT_CODE`]: start again at once.
    RestartRequested,
    /// Anything else, with its exit status (or 1 for a signal) and what it
    /// means.
    Crashed { code: i32, reason: String },
}

impl Outcome {
    pub fn of(status: ExitStatus) -> Self {
        match status.code() {
            Some(0) => Self::Stopped,
            Some(RESTART_EXIT_CODE) => Self::RestartRequested,
            code => Self::Crashed {
                code: code.unwrap_or(1),
                reason: describe_exit(status),
            },
        }
    }
}

/// What the supervisor does after a run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Next {
    Exit(i32),
    Restart(Duration),
}

/// When the backend starts again.
#[derive(Debug, Clone)]
pub struct Policy {
    pub backoff: Vec<Duration>,
    pub startup_grace: Duration,
    pub stable_after: Duration,
    crashes: usize,
    runs: u32,
}

impl Default for Policy {
    fn default() -> Self {
        Self::new(BACKOFF.to_vec(), STARTUP_GRACE, STABLE_AFTER)
    }
}

impl Policy {
    pub fn new(backoff: Vec<Duration>, startup_grace: Duration, stable_after: Duration) -> Self {
        Self {
            backoff,
            startup_grace,
            stable_after,
            crashes: 0,
            runs: 0,
        }
    }

    /// A run that lasted `ran_for` ended with `outcome`; `stopping` when the
    /// supervisor itself was asked to stop.
    pub fn next(&mut self, outcome: &Outcome, ran_for: Duration, stopping: bool) -> Next {
        let first = self.runs == 0;
        self.runs += 1;
        match outcome {
            Outcome::Stopped => Next::Exit(0),
            Outcome::RestartRequested if stopping => Next::Exit(0),
            Outcome::RestartRequested => Next::Restart(Duration::ZERO),
            Outcome::Crashed { code, .. } if stopping => Next::Exit(*code),
            Outcome::Crashed { code, .. } if first && ran_for < self.startup_grace => {
                Next::Exit(*code)
            }
            Outcome::Crashed { .. } => {
                if ran_for >= self.stable_after {
                    self.crashes = 0;
                }
                let last = self.backoff.len().saturating_sub(1);
                let pause = self
                    .backoff
                    .get(self.crashes.min(last))
                    .copied()
                    .unwrap_or_default();
                self.crashes += 1;
                Next::Restart(pause)
            }
        }
    }
}

/// Run `child` until it stops on request (or the supervisor is asked to
/// stop, `stop` turning true); returns the supervisor's exit status.
pub async fn supervise(child: &Child, mut policy: Policy, mut stop: watch::Receiver<bool>) -> i32 {
    let mut restarts: u32 = 0;
    let mut previous: Option<String> = None;
    loop {
        let started = Instant::now();
        let outcome = match spawn(child, restarts, previous.as_deref()) {
            Ok(mut process) => {
                tracing::info!(pid = process.id(), restarts, "Backend started");
                let mut passed_on = false;
                let status = loop {
                    tokio::select! {
                        status = process.wait() => break status,
                        () = stop_requested(&mut stop), if !passed_on => {
                            passed_on = true;
                            tracing::info!("Stop requested; waiting for the backend to stop");
                            forward_stop(&process);
                        }
                    }
                };
                match status {
                    Ok(status) => Outcome::of(status),
                    Err(error) => Outcome::Crashed {
                        code: 1,
                        reason: format!("could not wait for it: {error}"),
                    },
                }
            }
            Err(error) => Outcome::Crashed {
                code: 1,
                reason: format!("could not start it: {error}"),
            },
        };
        let stopping = *stop.borrow();
        let ran_for = started.elapsed();
        match policy.next(&outcome, ran_for, stopping) {
            Next::Exit(code) => {
                match &outcome {
                    Outcome::Crashed { reason, .. } if !stopping => tracing::error!(
                        reason = %reason,
                        ran_for_s = ran_for.as_secs(),
                        "The backend stopped right after starting; not starting it again"
                    ),
                    _ => tracing::info!(code, "Backend stopped; supervisor exiting"),
                }
                return code;
            }
            Next::Restart(pause) => {
                restarts += 1;
                match &outcome {
                    Outcome::Crashed { reason, .. } => {
                        tracing::error!(
                            reason = %reason,
                            ran_for_s = ran_for.as_secs(),
                            restart_in_s = pause.as_secs(),
                            restarts,
                            "The backend stopped unexpectedly; starting it again"
                        );
                        previous = Some(reason.clone());
                    }
                    _ => {
                        tracing::info!("The backend asked to be started again");
                        previous = None;
                    }
                }
                tokio::select! {
                    _ = tokio::time::sleep(pause) => {}
                    () = stop_requested(&mut stop) => {
                        tracing::info!("Stop requested; supervisor exiting");
                        return 0;
                    }
                }
            }
        }
    }
}

/// Resolves once the supervisor is asked to stop; never, if nothing can ask.
async fn stop_requested(stop: &mut watch::Receiver<bool>) {
    if stop.wait_for(|stop| *stop).await.is_err() {
        std::future::pending::<()>().await;
    }
}

fn spawn(
    child: &Child,
    restarts: u32,
    previous: Option<&str>,
) -> std::io::Result<tokio::process::Child> {
    let mut command = tokio::process::Command::new(&child.program);
    command
        .args(&child.args)
        .env(SUPERVISED_ENV, "1")
        .env(RESTARTS_ENV, restarts.to_string())
        .env(PREVIOUS_EXIT_ENV, previous.unwrap_or_default())
        .env(SUPERVISOR_PID_ENV, std::process::id().to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(false);
    #[cfg(windows)]
    {
        // The supervisor has no console; without this a console window opens.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command.spawn()
}

/// Pass a stop request on to the backend (it stops gracefully on SIGTERM).
/// On Windows the backend shares the request (Ctrl+C) or is stopped over IPC.
fn forward_stop(process: &tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = process.id().and_then(|pid| i32::try_from(pid).ok()) {
        // SAFETY: kill(2) with a child's PID and a valid signal number.
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
    }
    #[cfg(not(unix))]
    let _ = process;
}

async fn stop_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut terminate) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = terminate.recv() => {}
            }
            return;
        }
    }
    if tokio::signal::ctrl_c().await.is_err() {
        // No way to be told to stop: stop only with the backend.
        std::future::pending::<()>().await;
    }
}

/// What a backend's exit status means, for the log.
pub fn describe_exit(status: ExitStatus) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return describe_signal(signal);
        }
    }
    match status.code() {
        Some(code) => describe_code(code),
        None => "ended without an exit status".into(),
    }
}

/// An exit code; Windows reports a crash as its exception code (NTSTATUS).
pub fn describe_code(code: i32) -> String {
    let status = code as u32;
    if status < 0x4000_0000 {
        return format!("exit status {code}");
    }
    let meaning = match status {
        0xC000_0005 => Some("access violation: native code read or wrote invalid memory"),
        0xC000_00FD => Some("stack overflow"),
        0xC000_0409 => Some("fail-fast: a C/C++ library aborted or detected a buffer overrun"),
        0xC000_0374 => Some("heap corruption"),
        0xC000_001D => Some("illegal instruction"),
        0xC000_0094 => Some("integer division by zero"),
        0xC000_0017 | 0xC000_012D => Some("out of memory (the commit limit was reached)"),
        0xC000_0135 => Some("a required DLL was not found"),
        0xC000_0142 => Some("a DLL failed to initialize"),
        0xC000_013A => Some("stopped by Ctrl+C"),
        0xE06D_7363 => Some("unhandled C++ exception"),
        0x8000_0003 => Some("breakpoint: a library stopped on a fatal error"),
        0x4001_0004 => Some("terminated by the debugger or another program"),
        _ => None,
    };
    match meaning {
        Some(meaning) => format!("exit code {status:#010X}, {meaning}"),
        None => format!("exit code {status:#010X}"),
    }
}

#[cfg(unix)]
fn describe_signal(signal: i32) -> String {
    let meaning = match signal {
        libc::SIGSEGV => "SIGSEGV, invalid memory access",
        libc::SIGABRT => "SIGABRT, aborted by a panic or a library's fatal error",
        libc::SIGBUS => "SIGBUS, bus error",
        libc::SIGILL => "SIGILL, illegal instruction",
        libc::SIGFPE => "SIGFPE, arithmetic error",
        libc::SIGKILL => "SIGKILL, killed, possibly by the out-of-memory killer",
        libc::SIGTERM => "SIGTERM, terminated",
        _ => "",
    };
    if meaning.is_empty() {
        format!("signal {signal}")
    } else {
        format!("signal {signal}: {meaning}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    fn crash() -> Outcome {
        Outcome::Crashed {
            code: -1073741819,
            reason: "access violation".into(),
        }
    }

    fn test_policy() -> Policy {
        Policy::new(
            vec![2 * MS, 5 * MS, 10 * MS],
            Duration::from_secs(10),
            Duration::from_secs(600),
        )
    }

    #[test]
    fn a_backend_stopped_on_request_ends_the_supervisor() {
        let mut policy = test_policy();
        let ran = Duration::from_secs(3600);
        assert_eq!(policy.next(&Outcome::Stopped, ran, false), Next::Exit(0));
    }

    #[test]
    fn crashes_restart_after_growing_pauses_that_start_over_once_stable() {
        let mut policy = test_policy();
        let ran = Duration::from_secs(60);
        assert_eq!(policy.next(&crash(), ran, false), Next::Restart(2 * MS));
        assert_eq!(policy.next(&crash(), ran, false), Next::Restart(5 * MS));
        assert_eq!(policy.next(&crash(), ran, false), Next::Restart(10 * MS));
        assert_eq!(policy.next(&crash(), ran, false), Next::Restart(10 * MS));
        let long = Duration::from_secs(3600);
        assert_eq!(policy.next(&crash(), long, false), Next::Restart(2 * MS));
    }

    #[test]
    fn a_first_run_that_fails_at_once_is_a_failed_start() {
        let mut policy = test_policy();
        let quick = Duration::from_secs(1);
        assert_eq!(policy.next(&crash(), quick, false), Next::Exit(-1073741819));
        // Later quick failures are crashes: the backend ran before.
        let mut policy = test_policy();
        policy.next(&crash(), Duration::from_secs(60), false);
        assert_eq!(policy.next(&crash(), quick, false), Next::Restart(5 * MS));
    }

    #[test]
    fn a_requested_restart_is_immediate_and_a_stop_wins() {
        let mut policy = test_policy();
        let ran = Duration::from_secs(60);
        let restart = Outcome::RestartRequested;
        assert_eq!(
            policy.next(&restart, ran, false),
            Next::Restart(Duration::ZERO)
        );
        assert_eq!(policy.next(&restart, ran, true), Next::Exit(0));
        assert_eq!(policy.next(&crash(), ran, true), Next::Exit(-1073741819));
    }

    #[test]
    fn windows_crash_codes_are_named() {
        let access = describe_code(0xC000_0005_u32 as i32);
        assert!(
            access.starts_with("exit code 0xC0000005, access violation"),
            "{access}"
        );
        assert!(describe_code(0xC000_00FD_u32 as i32).contains("stack overflow"));
        assert_eq!(
            describe_code(0xC0FF_EE00_u32 as i32),
            "exit code 0xC0FFEE00"
        );
        assert_eq!(describe_code(1), "exit status 1");
    }

    #[cfg(unix)]
    #[test]
    fn signals_are_named() {
        use std::os::unix::process::ExitStatusExt;
        let segv = describe_exit(ExitStatus::from_raw(libc::SIGSEGV));
        assert!(segv.contains("SIGSEGV"), "{segv}");
        assert_eq!(describe_exit(ExitStatus::from_raw(3 << 8)), "exit status 3");
    }

    /// A real child: crashes twice (checking what it is told), then stops on
    /// request; the supervisor restarts it and exits with it.
    #[cfg(unix)]
    #[tokio::test]
    async fn supervise_restarts_a_crashed_child_until_it_stops_on_request() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time after the epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "neurowatch-supervise-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let runs = dir.join("runs");
        let script = format!(
            "echo \"$NW_SUPERVISED $NW_RESTARTS $NW_PREVIOUS_EXIT\" >> '{}'; \
             [ \"$NW_SUPERVISOR_PID\" = \"{}\" ] || exit 9; \
             n=$(wc -l < '{}'); [ \"$n\" -ge 3 ] && exit 0; kill -SEGV $$",
            runs.display(),
            std::process::id(),
            runs.display()
        );
        let child = Child {
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".into(), script.into()],
        };
        let policy = Policy::new(vec![MS], Duration::ZERO, Duration::from_secs(600));
        let (_stop_tx, stop_rx) = watch::channel(false);
        let code =
            tokio::time::timeout(Duration::from_secs(20), supervise(&child, policy, stop_rx))
                .await
                .expect("supervisor finished");
        assert_eq!(code, 0);
        let lines: Vec<String> = std::fs::read_to_string(&runs)
            .expect("runs")
            .lines()
            .map(str::to_owned)
            .collect();
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert_eq!(lines[0], "1 0 ");
        assert!(lines[1].starts_with("1 1 signal 11: SIGSEGV"), "{lines:?}");
        assert!(lines[2].starts_with("1 2 signal 11:"), "{lines:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_stop_request_is_passed_on_and_ends_the_supervisor() {
        let child = Child {
            program: PathBuf::from("/bin/sh"),
            // Exits 0 on SIGTERM, like the backend's graceful stop.
            args: vec![
                "-c".into(),
                "trap 'exit 0' TERM; while :; do sleep 0.05; done".into(),
            ],
        };
        let (stop_tx, stop_rx) = watch::channel(false);
        let run = tokio::spawn(async move {
            supervise(
                &child,
                Policy::new(vec![MS], Duration::ZERO, Duration::from_secs(600)),
                stop_rx,
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        stop_tx.send(true).expect("stop");
        let code = tokio::time::timeout(Duration::from_secs(10), run)
            .await
            .expect("supervisor finished")
            .expect("joined");
        assert_eq!(code, 0);
    }
}
