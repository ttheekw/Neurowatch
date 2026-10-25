//! NeuroWatch Cloud's clock, as seen from this computer.
//!
//! Every signed request carries `issued_at`/`expires_at`, and the cloud refuses
//! one whose times are more than a few seconds off its own clock. A recorder
//! whose clock is wrong — most often a Windows PC with the wrong time zone, so
//! its local time looks right while its UTC is off by hours — would otherwise
//! fail to link and fail every signed request after that.
//!
//! [`CloudClock`] learns the cloud's time from every response (the HTTP `Date`
//! header, or a precise `server_time` field where the portal sends one) and
//! corrects the times this node signs or checks against the cloud. Times on the
//! wire stay UTC; only the reference "now" changes.

use std::sync::Mutex;
use time::{Duration as TimeDuration, OffsetDateTime};

/// Offsets smaller than this are left alone: the `Date` header has one-second
/// resolution, and the cloud tolerates small differences anyway.
pub const APPLY_ABOVE: TimeDuration = TimeDuration::seconds(2);
/// Larger offsets are not adjusted for: the clock is too far off to be a time
/// zone mistake, and a cloud answer that far off is not trusted to set it.
pub const MAX_OFFSET: TimeDuration = TimeDuration::hours(48);
/// From this offset on the status report warns the operator.
pub const WARN_ABOVE: TimeDuration = TimeDuration::seconds(60);
/// A new sample that disagrees with the current estimate by more than its
/// uncertainty plus this much means this computer's clock was changed.
const SLACK: TimeDuration = TimeDuration::seconds(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Estimate {
    /// Cloud time minus this computer's time.
    offset: TimeDuration,
    /// Half the width of the interval the true offset lies in.
    uncertainty: TimeDuration,
}

#[derive(Debug, Default)]
struct State {
    estimate: Option<Estimate>,
    /// The latest sample beyond [`MAX_OFFSET`], kept for the warning only.
    unusable: Option<TimeDuration>,
}

/// The cloud's clock: this computer's UTC time plus a learned offset.
#[derive(Debug, Default)]
pub struct CloudClock {
    state: Mutex<State>,
    /// Tests: pretend this computer's clock is off by this much.
    #[cfg(test)]
    local_skew: Mutex<TimeDuration>,
}

impl CloudClock {
    pub fn new() -> Self {
        Self::default()
    }

    /// This computer's own UTC time, uncorrected.
    pub fn system_now(&self) -> OffsetDateTime {
        #[cfg(test)]
        {
            let skew = self
                .local_skew
                .lock()
                .map(|skew| *skew)
                .unwrap_or(TimeDuration::ZERO);
            OffsetDateTime::now_utc() + skew
        }
        #[cfg(not(test))]
        OffsetDateTime::now_utc()
    }

    /// The best estimate of the cloud's current time: what this node signs
    /// with and checks cloud-issued times against.
    pub fn now(&self) -> OffsetDateTime {
        self.system_now() + self.applied_offset()
    }

    /// The offset added to this computer's clock (zero while none is known,
    /// small, uncertain or beyond [`MAX_OFFSET`]).
    pub fn applied_offset(&self) -> TimeDuration {
        let Some(estimate) = self.estimate() else {
            return TimeDuration::ZERO;
        };
        if estimate.offset.abs() >= APPLY_ABOVE && estimate.offset.abs() > estimate.uncertainty {
            estimate.offset
        } else {
            TimeDuration::ZERO
        }
    }

    /// The measured offset (cloud minus this computer), once one is known.
    pub fn measured_offset(&self) -> Option<TimeDuration> {
        self.estimate().map(|estimate| estimate.offset)
    }

    fn estimate(&self) -> Option<Estimate> {
        self.state.lock().ok().and_then(|state| state.estimate)
    }

    /// Learn from a response whose cloud time is known to lie within
    /// `[server, server + resolution]`, for a request sent at `sent` and
    /// answered at `received` (both [`Self::system_now`]).
    pub fn observe(
        &self,
        server: OffsetDateTime,
        resolution: TimeDuration,
        sent: OffsetDateTime,
        received: OffsetDateTime,
    ) {
        if received < sent || resolution.is_negative() {
            return;
        }
        // The cloud stamped the answer at some moment between sending and
        // receiving, so the offset lies in [server - received, server + resolution - sent].
        let low = server - received;
        let high = server + resolution - sent;
        let sample = Estimate {
            offset: (low + high) / 2,
            uncertainty: (high - low) / 2,
        };
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if sample.offset.abs() > MAX_OFFSET {
            state.unusable = Some(sample.offset);
            return;
        }
        state.unusable = None;
        let keep = state.estimate.is_some_and(|current| {
            let consistent = (sample.offset - current.offset).abs()
                <= sample.uncertainty + current.uncertainty + SLACK;
            // A consistent but vaguer sample (a long poll held by the cloud)
            // teaches nothing new; an inconsistent one means the clock moved.
            consistent && sample.uncertainty >= current.uncertainty
        });
        if !keep {
            if state.estimate.map(|e| e.offset.whole_seconds())
                != Some(sample.offset.whole_seconds())
                && sample.offset.abs() >= APPLY_ABOVE
            {
                tracing::info!(
                    offset_ms = sample.offset.whole_milliseconds() as i64,
                    uncertainty_ms = sample.uncertainty.whole_milliseconds() as i64,
                    "This computer's clock differs from NeuroWatch Cloud; signing with the cloud's time"
                );
            }
            state.estimate = Some(sample);
        }
    }

    /// Learn from an HTTP `Date` header (IMF-fixdate, one-second resolution).
    pub fn observe_http_date(&self, value: &str, sent: OffsetDateTime, received: OffsetDateTime) {
        if let Ok(server) =
            OffsetDateTime::parse(value.trim(), &time::format_description::well_known::Rfc2822)
        {
            self.observe(server, TimeDuration::seconds(1), sent, received);
        }
    }

    /// Learn from a precise RFC 3339 `server_time` the portal sends.
    pub fn observe_server_time(&self, value: &str, sent: OffsetDateTime, received: OffsetDateTime) {
        if let Ok(server) =
            OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
        {
            // Allow for the portal truncating to the millisecond.
            self.observe(server, TimeDuration::milliseconds(1), sent, received);
        }
    }

    /// Learn from a response's headers (its `Date`).
    pub fn observe_headers(
        &self,
        headers: &reqwest::header::HeaderMap,
        sent: OffsetDateTime,
        received: OffsetDateTime,
    ) {
        if let Some(date) = headers
            .get(reqwest::header::DATE)
            .and_then(|value| value.to_str().ok())
        {
            self.observe_http_date(date, sent, received);
        }
    }

    /// A plain-language warning when this computer's clock is more than
    /// [`WARN_ABOVE`] off the cloud's.
    pub fn warning(&self) -> Option<String> {
        let (estimate, unusable) = self
            .state
            .lock()
            .map(|state| (state.estimate, state.unusable))
            .ok()?;
        if let Some(offset) = unusable {
            return Some(format!(
                "This computer's clock is {} NeuroWatch Cloud, too far off for NeuroWatch to \
                 adjust for, so the cloud link cannot work until it is corrected: {}",
                describe(offset),
                FIX_ADVICE
            ));
        }
        let offset = estimate?.offset;
        if offset.abs() <= WARN_ABOVE {
            return None;
        }
        Some(format!(
            "This computer's clock is {} NeuroWatch Cloud. NeuroWatch adjusted for it, but \
             other apps may misbehave: {}",
            describe(offset),
            FIX_ADVICE
        ))
    }

    #[cfg(test)]
    pub fn set_local_skew(&self, skew: TimeDuration) {
        if let Ok(mut current) = self.local_skew.lock() {
            *current = skew;
        }
    }
}

#[cfg(windows)]
const FIX_ADVICE: &str = "turn on \"Set time automatically\" and check the time zone in Windows \
    Settings → Time & language.";
#[cfg(not(windows))]
const FIX_ADVICE: &str = "turn on automatic (network) time and check the time zone in the \
    system's date and time settings.";

/// "5 h 30 min ahead of" / "3 min behind" for an offset (cloud minus local).
fn describe(offset: TimeDuration) -> String {
    let direction = if offset.is_negative() {
        "ahead of"
    } else {
        "behind"
    };
    format!("{} {direction}", amount(offset.abs()))
}

fn amount(span: TimeDuration) -> String {
    let minutes = (span.whole_seconds() + 30) / 60;
    let (days, hours, minutes) = (minutes / 1440, minutes / 60 % 24, minutes % 60);
    let mut parts = Vec::new();
    if days > 0 {
        parts.push(format!("{days} {}", if days == 1 { "day" } else { "days" }));
    }
    if hours > 0 {
        parts.push(format!("{hours} h"));
    }
    if minutes > 0 || parts.is_empty() {
        parts.push(format!("{minutes} min"));
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(raw: &str) -> OffsetDateTime {
        OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc3339).unwrap()
    }

    #[test]
    fn learns_the_offset_from_a_date_header_at_the_round_trip_midpoint() {
        let clock = CloudClock::new();
        // This computer thinks it is 17:30 UTC; the cloud says 12:00.
        clock.observe_http_date(
            "Sun, 25 Oct 2026 12:00:00 GMT",
            at("2026-10-25T17:30:00Z"),
            at("2026-10-25T17:30:00.200Z"),
        );
        let offset = clock.measured_offset().unwrap();
        // -5 h 30 min, within the header's one-second resolution.
        assert!((offset + TimeDuration::minutes(330)).abs() <= TimeDuration::seconds(1));
        assert_eq!(clock.applied_offset(), offset);
        let warning = clock.warning().unwrap();
        assert!(
            warning.contains("5 h 30 min ahead of NeuroWatch Cloud"),
            "{warning}"
        );
        assert!(warning.contains("adjusted for it"), "{warning}");
    }

    #[test]
    fn small_offsets_are_measured_but_not_applied_or_warned_about() {
        let clock = CloudClock::new();
        clock.observe_server_time(
            "2026-10-25T12:00:01Z",
            at("2026-10-25T12:00:00Z"),
            at("2026-10-25T12:00:00.100Z"),
        );
        assert!(clock.measured_offset().unwrap() > TimeDuration::ZERO);
        assert_eq!(clock.applied_offset(), TimeDuration::ZERO);
        assert_eq!(clock.warning(), None);
    }

    #[test]
    fn a_vaguer_long_poll_sample_does_not_replace_a_precise_one_but_a_clock_change_does() {
        let clock = CloudClock::new();
        clock.observe_server_time(
            "2026-10-25T12:00:10Z",
            at("2026-10-25T12:00:00Z"),
            at("2026-10-25T12:00:00.100Z"),
        );
        let precise = clock.measured_offset().unwrap();
        // A 15 s long poll: consistent, much vaguer.
        clock.observe_http_date(
            "Sun, 25 Oct 2026 12:00:30 GMT",
            at("2026-10-25T12:00:10Z"),
            at("2026-10-25T12:00:25Z"),
        );
        assert_eq!(clock.measured_offset(), Some(precise));
        // The computer's clock was corrected: the next quick answer disagrees.
        clock.observe_server_time(
            "2026-10-25T12:01:00Z",
            at("2026-10-25T12:01:00Z"),
            at("2026-10-25T12:01:00.050Z"),
        );
        assert!(clock.measured_offset().unwrap().abs() < TimeDuration::seconds(1));
        assert_eq!(clock.applied_offset(), TimeDuration::ZERO);
        assert_eq!(clock.warning(), None);
    }

    #[test]
    fn an_uncertain_estimate_is_not_applied() {
        let clock = CloudClock::new();
        // Answered after 20 s: the offset could be anywhere in a 20 s window.
        clock.observe_http_date(
            "Sun, 25 Oct 2026 12:00:03 GMT",
            at("2026-10-25T12:00:00Z"),
            at("2026-10-25T12:00:20Z"),
        );
        assert_eq!(clock.applied_offset(), TimeDuration::ZERO);
    }

    #[test]
    fn offsets_beyond_two_days_are_not_applied_but_are_explained() {
        let clock = CloudClock::new();
        clock.observe_server_time(
            "2026-10-20T12:00:00Z",
            at("2026-10-25T12:00:00Z"),
            at("2026-10-25T12:00:00.100Z"),
        );
        assert_eq!(clock.measured_offset(), None);
        assert_eq!(clock.applied_offset(), TimeDuration::ZERO);
        let warning = clock.warning().unwrap();
        assert!(warning.contains("5 days ahead of"), "{warning}");
        assert!(warning.contains("cannot work"), "{warning}");
    }

    #[test]
    fn a_computer_behind_the_cloud_is_described_as_behind() {
        let clock = CloudClock::new();
        clock.observe_server_time(
            "2026-10-25T12:03:00Z",
            at("2026-10-25T12:00:00Z"),
            at("2026-10-25T12:00:00.100Z"),
        );
        assert!(clock
            .warning()
            .unwrap()
            .contains("3 min behind NeuroWatch Cloud"));
        assert_eq!(amount(TimeDuration::seconds(61)), "1 min");
        assert_eq!(amount(TimeDuration::hours(26)), "1 day 2 h");
    }

    #[test]
    fn unparseable_or_inverted_samples_are_ignored() {
        let clock = CloudClock::new();
        clock.observe_http_date(
            "yesterday",
            at("2026-10-25T12:00:00Z"),
            at("2026-10-25T12:00:01Z"),
        );
        clock.observe_server_time(
            "2026-10-25T18:00:00Z",
            at("2026-10-25T12:00:01Z"),
            at("2026-10-25T12:00:00Z"),
        );
        assert_eq!(clock.measured_offset(), None);
        assert_eq!(clock.warning(), None);
    }
}
