//! Headroom for camera sockets, recordings, IPC clients and GStreamer wakeups.
//! This is capacity provisioning, not a remedy for a descriptor leak.

pub fn configure() {
    #[cfg(target_os = "linux")]
    match raise_nofile_soft_limit() {
        Ok((before, after)) => {
            tracing::info!(before, after, "Backend open-file soft limit configured");
        }
        Err(error) => {
            tracing::warn!(%error, "Could not raise the backend open-file soft limit");
        }
    }
}

#[cfg(target_os = "linux")]
fn raise_nofile_soft_limit() -> std::io::Result<(libc::rlim_t, libc::rlim_t)> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: limit is a valid, writable rlimit. No pointers outlive this call.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let before = limit.rlim_cur;
    if before < limit.rlim_max {
        // Preserve the administrator's hard limit; no privilege or system-wide
        // change is needed to raise this process's soft limit within it.
        limit.rlim_cur = limit.rlim_max;
        // SAFETY: limit is initialized, and setrlimit only reads it.
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok((before, limit.rlim_cur))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    #[test]
    fn raises_only_the_soft_limit_in_a_child_process() {
        const CHILD: &str = "NW_TEST_NOFILE_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "resource_limits::tests::raises_only_the_soft_limit_in_a_child_process",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(output.status.success(), "{:?}", output);
            return;
        }
        let mut original = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut original) },
            0
        );
        let lowered = libc::rlimit {
            rlim_cur: original.rlim_max.min(1024),
            rlim_max: original.rlim_max,
        };
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lowered) }, 0);
        assert_eq!(
            super::raise_nofile_soft_limit().unwrap(),
            (lowered.rlim_cur, original.rlim_max)
        );
        let mut actual = lowered;
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut actual) },
            0
        );
        assert_eq!(actual.rlim_cur, original.rlim_max);
        assert_eq!(actual.rlim_max, original.rlim_max);
        assert_eq!(
            super::raise_nofile_soft_limit().unwrap(),
            (original.rlim_max, original.rlim_max)
        );
    }
}
