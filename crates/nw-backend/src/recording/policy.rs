//! Recording policy primitives. Pure calendar/range logic; never deletes files
//! or changes a pipeline. Integration must retain continuous defaults.
pub use nw_core::camera::{RecordingMode, ScheduleRule};
const DAY: usize = 1440;
const WEEK: usize = DAY * 7;

pub fn minute_of_day(value: &str) -> Result<usize, &'static str> {
    if value.len() != 5 || value.as_bytes()[2] != b':' {
        return Err("Use HH:MM times");
    }
    let (hour, minute) = value.split_at(2);
    if !hour.bytes().all(|b| b.is_ascii_digit()) || !minute[1..].bytes().all(|b| b.is_ascii_digit())
    {
        return Err("Use HH:MM times");
    }
    let hour: usize = hour.parse().map_err(|_| "Invalid hour")?;
    let minute: usize = minute[1..].parse().map_err(|_| "Invalid minute")?;
    if hour > 23 || minute > 59 {
        return Err("Time is outside the day");
    }
    Ok(hour * 60 + minute)
}

fn grid(rules: &[ScheduleRule]) -> Result<Vec<Option<RecordingMode>>, &'static str> {
    if rules.len() > 64 {
        return Err("At most 64 schedule rules");
    }
    let mut slots = vec![None; WEEK];
    for rule in rules {
        if rule.days.is_empty() || rule.days.len() > 7 {
            return Err("Choose one or more days");
        }
        let start = minute_of_day(&rule.start)?;
        let end = minute_of_day(&rule.end)?;
        let duration = if end == start {
            DAY
        } else if end > start {
            end - start
        } else {
            DAY - start + end
        };
        let mut days = std::collections::HashSet::new();
        for day in &rule.days {
            if *day > 6 || !days.insert(day) {
                return Err("Days must be unique Monday=0 through Sunday=6");
            }
            // A full-day rule belongs to that calendar day, not start→next start.
            let start = if end == start { 0 } else { start };
            for offset in 0..duration {
                let i = (usize::from(*day) * DAY + start + offset) % WEEK;
                if slots[i].is_some() {
                    return Err("Schedule windows overlap");
                }
                slots[i] = Some(rule.mode);
            }
        }
    }
    Ok(slots)
}

pub fn validate_schedule(rules: &[ScheduleRule]) -> Result<(), &'static str> {
    grid(rules).map(|_| ())
}

pub fn validate_settings(
    settings: &nw_core::camera::CameraRecordingConfig,
) -> Result<(), &'static str> {
    validate_schedule(&settings.weekly_rules)?;
    if settings.event_pre_sec > 3600
        || settings.event_post_sec > 3600
        || settings.non_event_keep_hours > 87600
    {
        return Err(
            "Recording padding must be 0–3600 seconds; history retention at most ten years",
        );
    }
    Ok(())
}

/// Native local calendar, reevaluated each tick (including DST). No process
/// timezone/environment changes and no fixed UTC-offset approximation.
#[cfg(unix)]
pub fn local_calendar() -> Option<(u8, usize)> {
    local_calendar_at(time::OffsetDateTime::now_utc().unix_timestamp())
}

#[cfg(unix)]
fn local_calendar_at(timestamp: i64) -> Option<(u8, usize)> {
    // time_t is 32-bit on some supported Unix targets.
    #[allow(clippy::useless_conversion)]
    let now: libc::time_t = timestamp.try_into().ok()?;
    let mut calendar = std::mem::MaybeUninit::<libc::tm>::uninit();
    // SAFETY: valid time_t and writable tm storage; localtime_r initializes
    // tm on success and does not share a caller-visible static buffer.
    if unsafe { libc::localtime_r(&now, calendar.as_mut_ptr()) }.is_null() {
        return None;
    }
    let calendar = unsafe { calendar.assume_init() };
    Some((
        ((calendar.tm_wday + 6) % 7) as u8,
        (calendar.tm_hour * 60 + calendar.tm_min) as usize,
    ))
}
#[cfg(windows)]
pub fn local_calendar() -> Option<(u8, usize)> {
    let mut calendar =
        std::mem::MaybeUninit::<windows_sys::Win32::Foundation::SYSTEMTIME>::uninit();
    // SAFETY: GetLocalTime fills the supplied SYSTEMTIME.
    unsafe {
        windows_sys::Win32::System::SystemInformation::GetLocalTime(calendar.as_mut_ptr());
    }
    let calendar = unsafe { calendar.assume_init() };
    Some((
        ((calendar.wDayOfWeek + 6) % 7) as u8,
        usize::from(calendar.wHour) * 60 + usize::from(calendar.wMinute),
    ))
}
#[cfg(not(any(unix, windows)))]
pub fn local_calendar() -> Option<(u8, usize)> {
    None
}

pub fn current_mode(camera: &nw_core::camera::CameraConfig) -> RecordingMode {
    local_calendar()
        .map(|(day, minute)| {
            mode_at(
                camera.recording.mode,
                &camera.recording.weekly_rules,
                day,
                minute,
            )
        })
        .unwrap_or(camera.recording.mode)
}

/// Hot-path evaluation of already validated config; no weekly grid allocation
/// per camera per scheduler tick. Validation happens on load and mutation.
fn mode_at(
    default: RecordingMode,
    rules: &[ScheduleRule],
    day: u8,
    minute: usize,
) -> RecordingMode {
    for rule in rules {
        let (Ok(start), Ok(end)) = (minute_of_day(&rule.start), minute_of_day(&rule.end)) else {
            return default;
        };
        for selected in &rule.days {
            let active = if start == end {
                day == *selected
            } else if start < end {
                day == *selected && minute >= start && minute < end
            } else {
                (day == *selected && minute >= start)
                    || (day == (*selected + 1) % 7 && minute < end)
            };
            if active {
                return rule.mode;
            }
        }
    }
    default
}

/// Minutes on the local wall clock, not elapsed UTC seconds through DST.
pub fn next_change(
    default: RecordingMode,
    rules: &[ScheduleRule],
    day: u8,
    minute: usize,
) -> Result<Option<usize>, &'static str> {
    if day > 6 || minute >= DAY {
        return Err("Invalid local calendar time");
    }
    if rules.is_empty() {
        return Ok(None);
    }
    let slots = grid(rules)?;
    let current = usize::from(day) * DAY + minute;
    let mode = slots[current].unwrap_or(default);
    Ok((1..=WEEK).find(|offset| slots[(current + offset) % WEEK].unwrap_or(default) != mode))
}

pub fn effective_mode(
    default: RecordingMode,
    rules: &[ScheduleRule],
    weekday: u8,
    minute: usize,
) -> Result<RecordingMode, &'static str> {
    if weekday > 6 || minute >= DAY {
        return Err("Invalid local calendar time");
    }
    if rules.is_empty() {
        return Ok(default);
    }
    Ok(grid(rules)?[usize::from(weekday) * DAY + minute].unwrap_or(default))
}

/// Conservative closed boundaries keep adjacent files when an event lands
/// exactly at a segment edge. Open events extend forward until they end.
pub fn overlaps_event(
    start: i64,
    end: i64,
    event_start: i64,
    event_end: Option<i64>,
    pre_sec: u32,
    post_sec: u32,
) -> bool {
    if end < start || event_end.is_some_and(|end| end < event_start) {
        return false;
    }
    let from = event_start.saturating_sub(i64::from(pre_sec) * 1000);
    let through = event_end
        .map(|end| end.saturating_add(i64::from(post_sec) * 1000))
        .unwrap_or(i64::MAX);
    start <= through && end >= from
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[test]
    fn native_calendar_dst_gaps_repeated_hours_and_overnight_windows() {
        const CHILD: &str = "NW_RECORDING_DST_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "recording::policy::tests::native_calendar_dst_gaps_repeated_hours_and_overnight_windows", "--nocapture"])
                .env(CHILD, "1")
                .env("TZ", "EST5EDT,M3.2.0/2,M11.1.0/2")
                .output().unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        // Native localtime_r under an explicit POSIX zone in a child process:
        // never changes the timezone or clock of this process or the host.
        let calendar = |utc: &str| {
            local_calendar_at(
                time::OffsetDateTime::parse(utc, &time::format_description::well_known::Rfc3339)
                    .unwrap()
                    .unix_timestamp(),
            )
            .unwrap()
        };
        let rule = ScheduleRule {
            days: vec![6],
            start: "01:30".into(),
            end: "02:30".into(),
            mode: RecordingMode::Events,
        };
        let mode = |utc: &str| {
            let (day, minute) = calendar(utc);
            mode_at(RecordingMode::Off, std::slice::from_ref(&rule), day, minute)
        };
        assert_eq!(calendar("2026-03-08T06:59:00Z"), (6, 119));
        assert_eq!(calendar("2026-03-08T07:00:00Z"), (6, 180));
        assert_eq!(mode("2026-03-08T06:59:00Z"), RecordingMode::Events);
        assert_eq!(mode("2026-03-08T07:00:00Z"), RecordingMode::Off);
        for utc in ["2026-11-01T05:30:00Z", "2026-11-01T06:30:00Z"] {
            assert_eq!(calendar(utc), (6, 90));
            assert_eq!(mode(utc), RecordingMode::Events);
        }
        assert_eq!(mode("2026-11-01T07:30:00Z"), RecordingMode::Off);
        let overnight = ScheduleRule {
            days: vec![5],
            start: "22:15".into(),
            end: "03:15".into(),
            mode: RecordingMode::Continuous,
        };
        for utc in [
            "2026-03-08T06:59:00Z",
            "2026-03-08T07:00:00Z",
            "2026-11-01T05:30:00Z",
            "2026-11-01T06:30:00Z",
        ] {
            let (day, minute) = calendar(utc);
            assert_eq!(
                mode_at(
                    RecordingMode::Off,
                    std::slice::from_ref(&overnight),
                    day,
                    minute
                ),
                RecordingMode::Continuous
            );
        }
        let (day, minute) = calendar("2026-03-08T07:15:00Z");
        assert_eq!(
            mode_at(RecordingMode::Off, &[overnight], day, minute),
            RecordingMode::Off
        );
    }

    #[test]
    fn weekly_schedule_handles_overnight_week_wrap_and_end_boundaries() {
        let rule = ScheduleRule {
            days: vec![6],
            start: "22:00".into(),
            end: "02:00".into(),
            mode: RecordingMode::Events,
        };
        assert_eq!(
            effective_mode(
                RecordingMode::Continuous,
                std::slice::from_ref(&rule),
                6,
                23 * 60
            )
            .unwrap(),
            RecordingMode::Events
        );
        assert_eq!(
            effective_mode(
                RecordingMode::Continuous,
                std::slice::from_ref(&rule),
                0,
                60
            )
            .unwrap(),
            RecordingMode::Events
        );
        assert_eq!(
            effective_mode(RecordingMode::Continuous, &[rule], 0, 120).unwrap(),
            RecordingMode::Continuous
        );
    }
    #[test]
    fn malformed_schedules_and_overlapping_windows_are_not_silently_applied() {
        let invalid = ScheduleRule {
            days: vec![7],
            start: "08:00".into(),
            end: "18:00".into(),
            mode: RecordingMode::Off,
        };
        assert!(validate_schedule(&[invalid]).is_err());
        let rule = ScheduleRule {
            days: vec![0],
            start: "08:00".into(),
            end: "18:00".into(),
            mode: RecordingMode::Off,
        };
        assert!(validate_schedule(&[rule.clone(), rule]).is_err());
    }

    #[test]
    fn next_change_reports_wall_clock_minutes_and_matching_boundaries() {
        let rule = ScheduleRule {
            days: vec![6],
            start: "22:00".into(),
            end: "02:00".into(),
            mode: RecordingMode::Off,
        };
        assert_eq!(
            next_change(
                RecordingMode::Continuous,
                std::slice::from_ref(&rule),
                6,
                21 * 60
            )
            .unwrap(),
            Some(60)
        );
        assert_eq!(
            next_change(RecordingMode::Continuous, &[rule], 0, 60).unwrap(),
            Some(60)
        );
        assert_eq!(
            next_change(RecordingMode::Continuous, &[], 0, 60).unwrap(),
            None
        );
    }
    #[test]
    fn event_padding_keeps_open_events_and_bookmarks_but_not_unrelated_segments() {
        assert!(overlaps_event(10_000, 20_000, 25_000, Some(30_000), 10, 15));
        assert!(overlaps_event(50_000, 60_000, 25_000, None, 10, 15));
        assert!(!overlaps_event(
            100_000,
            120_000,
            25_000,
            Some(30_000),
            10,
            15
        ));
    }
}
