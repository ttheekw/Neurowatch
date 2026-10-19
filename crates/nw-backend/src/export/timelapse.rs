//! Keyframe-only, silent MP4 export. Child of remux: shares bounded readers,
//! codec checks, MP4 writing, cancellation and finalization with clip export.
use super::*;
use crate::export::TimelapseOptions;

pub fn remux_timelapse(
    segments: &[PlaybackSegment],
    start_ms: i64,
    end_ms: i64,
    output: &Path,
    options: TimelapseOptions,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(f64),
) -> Result<RemuxSummary, ExportError> {
    options.validate(start_ms, end_ms)?;
    let scratch = sibling(output, ".moov");
    let result = (|| {
        let mut writer: Option<Writer> = None;
        let mut first_caps: Option<gstreamer::Caps> = None;
        let mut previous_wall: Option<i64> = None;
        let mut next_wall = start_ms.saturating_mul(1000);
        let mut coverage_end: Option<i64> = None;
        let mut skipped = 0;
        let mut truncated = false;
        'files: for (index, segment) in segments.iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                return Err(ExportError::Cancelled);
            }
            if segment.end_ts <= start_ms || segment.start_ts >= end_ms {
                continue;
            }
            let mut gap_pending =
                coverage_end.is_some_and(|end| segment.start_ts > end + MAX_KEPT_GAP_US / 1000);
            coverage_end = Some(coverage_end.unwrap_or(segment.end_ts).max(segment.end_ts));
            let Ok((pipeline, slot, _)) = open_file(&segment.path, &SoundPlan::Off) else {
                skipped += 1;
                continue;
            };
            let source = Source(pipeline);
            source
                .0
                .set_state(gstreamer::State::Playing)
                .map_err(|_| ExportError::Unreadable)?;
            let bus = source.0.bus().ok_or(ExportError::Unreadable)?;
            let mut last_data = Instant::now();
            let mut found_sample = false;
            loop {
                if cancel.load(Ordering::Relaxed) {
                    return Err(ExportError::Cancelled);
                }
                let found = slot.lock().ok().and_then(|s| s.clone());
                let (sink, codec) = match found {
                    Some(Ok(found)) => found,
                    Some(Err(codec)) => {
                        if writer.is_some() {
                            truncated = true;
                            break 'files;
                        }
                        return Err(ExportError::Unsupported(codec));
                    }
                    None => {
                        if bus
                            .pop_filtered(&[
                                gstreamer::MessageType::Error,
                                gstreamer::MessageType::Eos,
                            ])
                            .is_some()
                            || last_data.elapsed() > OPEN_TIMEOUT
                        {
                            skipped += 1;
                            break;
                        }
                        std::thread::sleep(SLICE);
                        continue;
                    }
                };
                if !matches!(codec, VideoCodec::H264 | VideoCodec::H265) {
                    return Err(ExportError::Unsupported(format!("{codec:?}")));
                }
                let Some(sample) = sink.try_pull_sample(PULL_TIMEOUT) else {
                    if sink.is_eos() {
                        if !found_sample {
                            skipped += 1;
                        }
                        break;
                    }
                    if bus.pop_filtered(&[gstreamer::MessageType::Error]).is_some()
                        || last_data.elapsed() > OPEN_TIMEOUT
                    {
                        return Err(ExportError::Unreadable);
                    }
                    continue;
                };
                found_sample = true;
                last_data = Instant::now();
                let Some(unit) = Unit::from_sample(&sample, segment.start_ts.saturating_mul(1000))
                else {
                    continue;
                };
                // Skip overlaps and sample at/after the requested instant. No
                // early keyframe and no frozen-gap filler frames in a timelapse.
                if !unit.keyframe
                    || unit.wall_us < next_wall
                    || previous_wall.is_some_and(|last| unit.wall_us <= last)
                {
                    continue;
                }
                if unit.wall_us >= end_ms.saturating_mul(1000) {
                    break;
                }
                let mut caps = sample.caps().ok_or(ExportError::Unreadable)?.to_owned();
                caps.make_mut()
                    .set("framerate", gstreamer::Fraction::new(options.fps as i32, 1));
                if let Some(first) = &first_caps {
                    if first != &caps {
                        truncated = true;
                        break 'files;
                    }
                } else {
                    writer = Some(Writer::new(&caps, codec, None, output, &scratch)?);
                    first_caps = Some(caps);
                }
                let out = writer.as_mut().ok_or(ExportError::Unreadable)?;
                if out.frames >= crate::export::MAX_TIMELAPSE_FRAMES {
                    return Err(ExportError::Invalid(
                        "Too many timelapse pictures; use a longer sampling interval".into(),
                    ));
                }
                if gap_pending {
                    out.gaps += 1;
                    gap_pending = false;
                }
                let pts = out.frames * 1_000_000_000 / u64::from(options.fps);
                let end = (out.frames + 1) * 1_000_000_000 / u64::from(options.fps);
                let mut buffer = unit.buffer;
                let b = buffer.make_mut();
                b.set_pts(gstreamer::ClockTime::from_nseconds(pts));
                b.set_dts(gstreamer::ClockTime::from_nseconds(pts));
                b.set_duration(gstreamer::ClockTime::from_nseconds(end - pts));
                b.set_offset(out.frames);
                b.set_offset_end(out.frames + 1);
                out.push_video(buffer)?;
                out.throttle(cancel)?;
                out.first_wall_us.get_or_insert(unit.wall_us);
                out.last_wall_us = unit.wall_us;
                out.end_us = (end / 1000) as i64;
                out.frames += 1;
                previous_wall = Some(unit.wall_us);
                next_wall = unit.wall_us + i64::from(options.interval_sec) * 1_000_000;
                progress(
                    (index as f64
                        + (unit.wall_us / 1000 - segment.start_ts) as f64
                            / (segment.end_ts - segment.start_ts).max(1) as f64)
                        .clamp(0.0, segments.len() as f64)
                        / segments.len().max(1) as f64,
                );
            }
        }
        let out = writer.ok_or(if skipped > 0 {
            ExportError::Unreadable
        } else {
            ExportError::NoRecordings
        })?;
        let mut summary = out.finish(cancel)?;
        summary.skipped_files = skipped;
        summary.truncated = truncated;
        Ok(summary)
    })();
    let _ = std::fs::remove_file(scratch);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn timelapse_range_and_sampling_limits_are_separate_from_normal_clips() {
        let options = TimelapseOptions {
            interval_sec: 10,
            fps: 25,
        };
        assert!(options.validate(0, 48 * 60 * 60 * 1000).is_ok());
        assert!(crate::export::validate_range(0, 48 * 60 * 60 * 1000).is_err());
        assert!(TimelapseOptions {
            interval_sec: 0,
            fps: 25
        }
        .validate(0, 1000)
        .is_err());
        assert!(TimelapseOptions {
            interval_sec: 1,
            fps: 61
        }
        .validate(0, 1000)
        .is_err());
        assert!(TimelapseOptions {
            interval_sec: 1,
            fps: 25
        }
        .validate(0, 31 * 24 * 60 * 60 * 1000)
        .is_err());
    }
    #[test]
    fn timelapse_keyframes_decode_at_fixed_rate_without_audio_in_h264_and_h265() {
        for (parser, encoder, decoder) in [
            ("h264parse","x264enc speed-preset=ultrafast key-int-max=25 bframes=2", "avdec_h264"),
            ("h265parse","x265enc speed-preset=ultrafast key-int-max=25 option-string=pools=1:frame-threads=1", "avdec_h265")
        ] {
            gstreamer::init().unwrap();
            let root=std::env::temp_dir().join(format!("nw-timelapse-{}",uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&root).unwrap();
            let input=root.join("recording.mkv");let output=root.join("timelapse.mp4");
            let source=builder::build(&format!("videotestsrc num-buffers=250 ! video/x-raw,format=I420,width=160,height=120,framerate=25/1 ! {encoder} ! {parser} ! matroskamux name=mux ! filesink location=\"{}\" audiotestsrc num-buffers=100 samplesperbuffer=800 ! audio/x-raw,rate=8000,channels=1 ! alawenc ! mux.",input.display())).unwrap();
            source.set_state(gstreamer::State::Playing).unwrap();
            let done=source.bus().unwrap().timed_pop_filtered(gstreamer::ClockTime::from_seconds(30),&[gstreamer::MessageType::Eos,gstreamer::MessageType::Error]);
            source.set_state(gstreamer::State::Null).unwrap();
            assert!(done.is_some_and(|m|m.type_()==gstreamer::MessageType::Eos));
            let segment=PlaybackSegment{path:input.display().to_string(),start_ts:0,end_ts:10_000};
            let cancel=AtomicBool::new(false);
            let summary=remux_timelapse(&[segment],1500,9500,&output,TimelapseOptions{interval_sec:2,fps:10},&cancel,&mut |_|{}).unwrap();
            assert_eq!(summary.frames,4,"{parser}: {summary:?}");
            assert_eq!(summary.duration_ms,400);
            assert!(!summary.audio);
            assert!((summary.first_ms-2000).abs()<5);
            let decode=builder::build(&format!("filesrc location=\"{}\" ! qtdemux ! {parser} ! {decoder} ! fakesink name=count sync=false",output.display())).unwrap();
            let frames=Arc::new(std::sync::atomic::AtomicU64::new(0));let count=Arc::clone(&frames);
            decode.by_name("count").unwrap().static_pad("sink").unwrap().add_probe(gstreamer::PadProbeType::BUFFER,move|_,_|{count.fetch_add(1,Ordering::Relaxed);gstreamer::PadProbeReturn::Ok});
            decode.set_state(gstreamer::State::Playing).unwrap();
            let done=decode.bus().unwrap().timed_pop_filtered(gstreamer::ClockTime::from_seconds(20),&[gstreamer::MessageType::Eos,gstreamer::MessageType::Error]);
            decode.set_state(gstreamer::State::Null).unwrap();
            assert!(done.is_some_and(|m|m.type_()==gstreamer::MessageType::Eos));
            assert_eq!(frames.load(Ordering::Relaxed),4,"every sampled picture independently decodes");
            std::fs::remove_dir_all(root).unwrap();
        }
    }
}

#[cfg(test)]
mod additional_tests {
    use super::*;
    #[test]
    fn timelapse_handles_overlap_gaps_cancellation_and_shared_job_status() {
        let input = super::super::tests::recording(4, false);
        let root = std::env::temp_dir().join(format!("nw-timelapse-job-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let output = root.join("sampled.mp4");
        let segments = vec![
            PlaybackSegment {
                path: input.clone(),
                start_ts: 0,
                end_ts: 4000,
            },
            PlaybackSegment {
                path: input.clone(),
                start_ts: 2000,
                end_ts: 6000,
            },
            PlaybackSegment {
                path: input.clone(),
                start_ts: 10000,
                end_ts: 14000,
            },
        ];
        let options = TimelapseOptions {
            interval_sec: 1,
            fps: 5,
        };
        let summary = remux_timelapse(
            &segments,
            0,
            14000,
            &output,
            options,
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(
            summary.frames, 10,
            "overlapping file times must not duplicate pictures"
        );
        assert_eq!(
            summary.duration_ms, 2000,
            "gaps do not make frozen filler pictures"
        );
        assert_eq!(summary.gaps, 1);
        let cancelled = root.join("cancelled.mp4");
        assert!(matches!(
            remux_timelapse(
                &segments,
                0,
                14000,
                &cancelled,
                options,
                &AtomicBool::new(true),
                &mut |_| {}
            ),
            Err(ExportError::Cancelled)
        ));
        assert!(!cancelled.exists());
        let request = crate::export::ExportRequest {
            camera_id: "fixture".into(),
            camera_name: "Fixture".into(),
            start_ms: 0,
            end_ms: 14000,
            output: Some(root.join("job.mp4")),
            overwrite: false,
        };
        let service = crate::export::ExportService::new();
        let job = service
            .start_timelapse(&request, segments, options)
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let status = service.status(&job.job_id).unwrap();
            if status.state != "running" {
                assert_eq!(status.state, "done", "{status:?}");
                assert_eq!(status.frames, Some(10));
                assert_eq!(status.duration_ms, Some(2000));
                assert_eq!(status.audio, Some(false));
                assert!(Path::new(&status.path).exists());
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!root.join("job.mp4.part").exists());
        std::fs::remove_file(input).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
}
