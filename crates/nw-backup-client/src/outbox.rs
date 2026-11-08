//! One track's units from numbering until the ingest confirms them.
//!
//! Units wait in memory up to a budget; beyond it the oldest move to the
//! outage folder (append-only spill files, deleted once every unit in them is
//! confirmed or given up). Beyond the folder's budget, and once older than
//! the camera's backup window, the oldest are given up and reported as a gap.
//!
//! Sending: after a (re)connection, live sending starts at the next keyframe
//! pushed, so a picture group never splits between live and backlog; every
//! unconfirmed unit before it is the backlog, sent oldest first at a lower
//! priority. A live sender that falls too far behind jumps to the newest
//! keyframe and leaves what it skipped to the backlog.
use nw_backup_protocol::{
    frame::{self, MediaKind, MediaUnit},
    messages::GapReason,
    ranges::{SeqRange, SeqRanges},
};
use std::{
    collections::{BTreeMap, HashMap},
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::PathBuf,
};

/// A spill file is closed for appending at this size.
const SPILL_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// A live sender this far behind the newest unit jumps ahead.
pub const LIVE_LAG_US: i64 = 5_000_000;

#[derive(Debug, Clone)]
pub struct Budget {
    /// Units kept in memory before the oldest move to disk.
    pub memory_bytes: usize,
    /// The outage folder's share for this track; 0: no folder.
    pub disk_bytes: u64,
    /// Where this track's spill files go.
    pub dir: Option<PathBuf>,
}

/// Units given up, merged into one report per reason and run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GivenUp {
    pub seqs: SeqRange,
    pub from_time_us: i64,
    pub to_time_us: i64,
    pub reason: GapReason,
}

#[derive(Debug)]
struct Entry {
    /// `data` is empty while the unit is on disk.
    unit: MediaUnit,
    spilled: Option<(u32, u64, u32)>,
    bytes: usize,
    sent: bool,
    /// When the keyframe that started this unit's picture group arrived.
    gop_us: i64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    pub units: usize,
    pub memory_bytes: usize,
    pub disk_bytes: u64,
    /// Arrival time of the oldest unconfirmed unit.
    pub oldest_us: Option<i64>,
}

pub struct Outbox {
    pub epoch: u64,
    media: MediaKind,
    budget: Budget,
    next_seq: u64,
    entries: BTreeMap<u64, Entry>,
    memory: usize,
    disk: u64,
    /// Open spill files: number → (file, live units in it, size).
    files: HashMap<u32, (File, usize, u64)>,
    writing: Option<u32>,
    next_file: u32,
    given_up: Vec<GivenUp>,
    /// While connected: the first live unit (a keyframe), once pushed.
    live_from: Option<u64>,
    connected: bool,
    gop_us: i64,
    /// Keep the spill files when dropped (a clean stop: sent after the restart).
    keep_files: bool,
}

/// A spill file's name.
fn spill_name(epoch: u64, number: u32) -> String {
    format!("{epoch:016x}-{number}.spill")
}

impl Outbox {
    pub fn new(media: MediaKind, epoch: u64, budget: Budget) -> Self {
        Self {
            epoch,
            media,
            budget,
            next_seq: 1,
            entries: BTreeMap::new(),
            memory: 0,
            disk: 0,
            files: HashMap::new(),
            writing: None,
            next_file: 0,
            given_up: Vec::new(),
            live_from: None,
            connected: false,
            gop_us: i64::MIN,
            keep_files: false,
        }
    }

    /// A track a previous run left in the outage folder: every unit in its
    /// spill files, with its number, as backlog. A cut-off last frame (a
    /// crash while writing) is ignored.
    pub fn recover(media: MediaKind, epoch: u64, budget: Budget) -> io::Result<Self> {
        let mut outbox = Self::new(media, epoch, budget);
        let dir = outbox
            .budget
            .dir
            .clone()
            .ok_or_else(|| io::Error::other("no outage folder"))?;
        let mut numbers: Vec<u32> = fs::read_dir(&dir)?
            .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
            .filter_map(|name| {
                let rest = name.strip_prefix(&format!("{epoch:016x}-"))?;
                rest.strip_suffix(".spill")?.parse().ok()
            })
            .collect();
        numbers.sort_unstable();
        // After a clean stop, only what was still unconfirmed (the files are
        // append-only); after a crash, everything (the ingest skips what it has).
        let kept: Option<SeqRanges> = fs::read(dir.join(kept_name(epoch)))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Vec<SeqRange>>(&bytes).ok())
            .map(|ranges| SeqRanges::from_wire(&ranges));
        for number in numbers {
            let path = dir.join(spill_name(epoch, number));
            let bytes = fs::read(&path)?;
            let file = OpenOptions::new().read(true).write(true).open(&path)?;
            let mut count = 0;
            let mut input = bytes.as_slice();
            loop {
                let offset = (bytes.len() - input.len()) as u64;
                let Ok(frame::Frame::Media(mut unit)) = frame::read_frame(&mut input) else {
                    break;
                };
                let len = (bytes.len() - input.len()) as u64 - offset;
                if unit.keyframe() || media == MediaKind::Audio {
                    outbox.gop_us = unit.time_us;
                }
                let size = unit.data.len();
                unit.data = Vec::new();
                outbox.next_seq = outbox.next_seq.max(unit.seq + 1);
                if kept.as_ref().is_some_and(|kept| !kept.contains(unit.seq)) {
                    continue;
                }
                let gop_us = if outbox.gop_us == i64::MIN {
                    unit.time_us
                } else {
                    outbox.gop_us
                };
                outbox.entries.insert(
                    unit.seq,
                    Entry {
                        unit,
                        spilled: Some((number, offset, len as u32)),
                        bytes: size,
                        sent: false,
                        gop_us,
                    },
                );
                outbox.disk += size as u64;
                count += 1;
            }
            outbox
                .files
                .insert(number, (file, count, bytes.len() as u64));
            outbox.next_file = outbox.next_file.max(number + 1);
        }
        Ok(outbox)
    }

    /// A clean stop: move what is in memory to the outage folder and keep
    /// the files, so the next run sends them. Without a folder, nothing.
    pub fn persist(&mut self) {
        if self.budget.dir.is_none() || self.budget.disk_bytes == 0 {
            return;
        }
        let in_memory: Vec<u64> = self
            .entries
            .iter()
            .filter(|(_, e)| e.spilled.is_none())
            .map(|(&s, _)| s)
            .collect();
        for seq in in_memory {
            if self.disk + self.entries[&seq].bytes as u64 > self.budget.disk_bytes {
                break;
            }
            if let Err(error) = self.spill(seq) {
                tracing::warn!(%error, "unconfirmed units could not be kept for the next start");
                break;
            }
        }
        self.keep_files = !self.entries.is_empty();
        if self.keep_files {
            let mut kept = SeqRanges::new();
            for &seq in self.entries.keys() {
                kept.insert(seq);
            }
            if let Some(dir) = &self.budget.dir {
                let wire: Vec<SeqRange> = kept.iter().map(|(a, b)| SeqRange(a, b)).collect();
                let written = serde_json::to_vec(&wire)
                    .map_err(io::Error::other)
                    .and_then(|json| fs::write(dir.join(kept_name(self.epoch)), json));
                if let Err(error) = written {
                    tracing::warn!(%error, "the outage folder's index could not be written");
                }
            }
        }
    }

    /// Number and keep a unit; returns its number.
    pub fn push(&mut self, mut unit: MediaUnit) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        unit.seq = seq;
        unit.media = self.media;
        let keyframe = unit.keyframe() || self.media == MediaKind::Audio;
        if keyframe {
            self.gop_us = unit.time_us;
        }
        if self.connected && self.live_from.is_none() && keyframe {
            self.live_from = Some(seq);
        }
        let bytes = unit.data.len();
        self.memory += bytes;
        self.entries.insert(
            seq,
            Entry {
                gop_us: if self.gop_us == i64::MIN {
                    unit.time_us
                } else {
                    self.gop_us
                },
                unit,
                spilled: None,
                bytes,
                sent: false,
            },
        );
        self.enforce_budget();
        seq
    }

    /// The ingest holds `durable`: forget those units.
    pub fn confirmed(&mut self, durable: &SeqRanges) {
        let done: Vec<u64> = durable
            .iter()
            .flat_map(|(start, end)| {
                self.entries
                    .range(start..=end)
                    .map(|(&seq, _)| seq)
                    .collect::<Vec<_>>()
            })
            .collect();
        for seq in done {
            self.forget(seq);
        }
    }

    /// Give up units that arrived before `oldest_us` (older than the window).
    pub fn expire(&mut self, oldest_us: i64) {
        let old: Vec<u64> = self
            .entries
            .iter()
            .take_while(|(_, e)| e.unit.time_us < oldest_us)
            .map(|(&seq, _)| seq)
            .collect();
        for seq in old {
            self.give_up(seq, GapReason::Expired);
        }
    }

    /// Gaps not yet reported.
    pub fn take_given_up(&mut self) -> Vec<GivenUp> {
        std::mem::take(&mut self.given_up)
    }

    /// A session started: live sending waits for the next keyframe.
    pub fn connected(&mut self) {
        self.connected = true;
        self.live_from = None;
        for entry in self.entries.values_mut() {
            entry.sent = false;
        }
    }

    /// The session ended: everything unconfirmed is sent again next time.
    pub fn disconnected(&mut self) {
        self.connected = false;
        self.live_from = None;
        for entry in self.entries.values_mut() {
            entry.sent = false;
        }
    }

    /// The next live unit after `after` (unsent), jumping to the newest
    /// keyframe when the live sender is over [`LIVE_LAG_US`] behind.
    pub fn next_live(&mut self, after: Option<u64>) -> io::Result<Option<(MediaUnit, i64)>> {
        let Some(live_from) = self.live_from else {
            return Ok(None);
        };
        let newest = self.entries.values().next_back().map(|e| e.unit.time_us);
        let start = after.map_or(live_from, |a| (a + 1).max(live_from));
        let next = self
            .entries
            .range(start..)
            .find(|(_, e)| !e.sent)
            .map(|(&seq, e)| (seq, e.unit.time_us));
        let Some((mut seq, time_us)) = next else {
            return Ok(None);
        };
        if newest.is_some_and(|newest| newest - time_us > LIVE_LAG_US) {
            let jump = self
                .entries
                .range(seq..)
                .rev()
                .find(|(_, e)| !e.sent && (e.unit.keyframe() || self.media == MediaKind::Audio))
                .map(|(&s, _)| s);
            if let Some(jump) = jump {
                if jump > seq {
                    self.live_from = Some(jump);
                    seq = jump;
                }
            }
        }
        self.load(seq)
    }

    /// The oldest unsent unit before live sending's start.
    pub fn next_backlog(&mut self) -> io::Result<Option<(MediaUnit, i64)>> {
        let limit = self.live_from.unwrap_or(u64::MAX);
        let seq = self
            .entries
            .range(..limit)
            .find(|(_, e)| !e.sent)
            .map(|(&seq, _)| seq);
        match seq {
            Some(seq) => self.load(seq),
            None => Ok(None),
        }
    }

    pub fn mark_sent(&mut self, seq: u64) {
        if let Some(entry) = self.entries.get_mut(&seq) {
            entry.sent = true;
        }
    }

    pub fn counts(&self) -> Counts {
        Counts {
            units: self.entries.len(),
            memory_bytes: self.memory,
            disk_bytes: self.disk,
            oldest_us: self.entries.values().next().map(|e| e.unit.time_us),
        }
    }

    /// The unit (read back from disk when spilled) and its group's start.
    fn load(&mut self, seq: u64) -> io::Result<Option<(MediaUnit, i64)>> {
        let Some(entry) = self.entries.get(&seq) else {
            return Ok(None);
        };
        let gop_us = entry.gop_us;
        let Some((file, offset, len)) = entry.spilled else {
            return Ok(Some((entry.unit.clone(), gop_us)));
        };
        let mut unit = entry.unit.clone();
        let (handle, _, _) = self
            .files
            .get_mut(&file)
            .ok_or_else(|| io::Error::other("spill file missing"))?;
        handle.seek(SeekFrom::Start(offset))?;
        let mut bytes = vec![0u8; len as usize];
        handle.read_exact(&mut bytes)?;
        match frame::read_frame(&mut bytes.as_slice())? {
            frame::Frame::Media(read) if read.seq == seq => unit.data = read.data,
            _ => return Err(io::Error::other("spill file does not match its index")),
        }
        Ok(Some((unit, gop_us)))
    }

    fn enforce_budget(&mut self) {
        while self.memory > self.budget.memory_bytes {
            let oldest = self
                .entries
                .iter()
                .find(|(_, e)| e.spilled.is_none())
                .map(|(&s, _)| s);
            let Some(seq) = oldest else { break };
            if self.budget.disk_bytes == 0 || self.budget.dir.is_none() {
                self.give_up(seq, GapReason::OutageFull);
                continue;
            }
            while self.disk + self.entries[&seq].bytes as u64 > self.budget.disk_bytes {
                let Some(&first) = self.entries.keys().next() else {
                    break;
                };
                self.give_up(first, GapReason::OutageFull);
                if first == seq {
                    break;
                }
            }
            if !self.entries.contains_key(&seq) {
                continue;
            }
            if let Err(error) = self.spill(seq) {
                tracing::warn!(%error, "the outage folder could not be written: units given up");
                self.give_up(seq, GapReason::OutageFull);
            }
        }
    }

    fn spill(&mut self, seq: u64) -> io::Result<()> {
        let dir = self
            .budget
            .dir
            .clone()
            .ok_or_else(|| io::Error::other("no outage folder"))?;
        let number = match self.writing {
            Some(n) if self.files.get(&n).is_some_and(|f| f.2 < SPILL_FILE_BYTES) => n,
            _ => {
                fs::create_dir_all(&dir)?;
                let n = self.next_file;
                self.next_file += 1;
                let path = dir.join(spill_name(self.epoch, n));
                let file = OpenOptions::new()
                    .create(true)
                    .truncate(true)
                    .read(true)
                    .write(true)
                    .open(path)?;
                self.files.insert(n, (file, 0, 0));
                self.writing = Some(n);
                n
            }
        };
        let entry = self
            .entries
            .get_mut(&seq)
            .ok_or_else(|| io::Error::other("no such unit"))?;
        let mut bytes = Vec::with_capacity(entry.bytes + 64);
        frame::encode_media(&mut bytes, &entry.unit)?;
        let (file, count, size) = self
            .files
            .get_mut(&number)
            .ok_or_else(|| io::Error::other("spill file missing"))?;
        let offset = *size;
        file.seek(SeekFrom::Start(offset))?;
        file.write_all(&bytes)?;
        *size += bytes.len() as u64;
        *count += 1;
        entry.spilled = Some((number, offset, bytes.len() as u32));
        entry.unit.data = Vec::new();
        self.memory -= entry.bytes;
        self.disk += entry.bytes as u64;
        Ok(())
    }

    fn give_up(&mut self, seq: u64, reason: GapReason) {
        let Some(time_us) = self.entries.get(&seq).map(|e| e.unit.time_us) else {
            return;
        };
        self.forget(seq);
        match self.given_up.last_mut() {
            Some(last) if last.reason == reason && last.seqs.1 + 1 == seq => {
                last.seqs.1 = seq;
                last.to_time_us = time_us;
            }
            _ => self.given_up.push(GivenUp {
                seqs: SeqRange(seq, seq),
                from_time_us: time_us,
                to_time_us: time_us,
                reason,
            }),
        }
    }

    fn forget(&mut self, seq: u64) {
        let Some(entry) = self.entries.remove(&seq) else {
            return;
        };
        match entry.spilled {
            None => self.memory -= entry.bytes,
            Some((number, _, _)) => {
                self.disk -= entry.bytes as u64;
                let empty = self.files.get_mut(&number).is_some_and(|(_, count, _)| {
                    *count -= 1;
                    *count == 0
                });
                if empty && self.writing != Some(number) {
                    self.files.remove(&number);
                    if let Some(dir) = &self.budget.dir {
                        let _ = fs::remove_file(
                            dir.join(format!("{:016x}-{number}.spill", self.epoch)),
                        );
                    }
                }
            }
        }
    }

    /// Remove this track's spill files (the track is gone).
    pub fn discard(&mut self) {
        self.entries.clear();
        self.files.clear();
        if let Some(dir) = &self.budget.dir {
            for number in 0..self.next_file {
                let _ = fs::remove_file(dir.join(spill_name(self.epoch, number)));
            }
            let _ = fs::remove_file(dir.join(description_name(self.epoch)));
            let _ = fs::remove_file(dir.join(kept_name(self.epoch)));
        }
        self.memory = 0;
        self.disk = 0;
        self.keep_files = false;
    }
}

/// What a clean stop left unconfirmed.
fn kept_name(epoch: u64) -> String {
    format!("{epoch:016x}.kept.json")
}

/// The file describing a track whose units are in the outage folder.
pub fn description_name(epoch: u64) -> String {
    format!("{epoch:016x}.track.json")
}

impl Drop for Outbox {
    fn drop(&mut self) {
        if !self.keep_files {
            self.discard();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nw_backup_protocol::frame::flags;

    fn unit(time_us: i64, keyframe: bool, bytes: usize) -> MediaUnit {
        MediaUnit {
            media: MediaKind::Video,
            flags: if keyframe { flags::KEYFRAME } else { 0 },
            track: 1,
            seq: 0,
            time_us,
            pts_ns: Some(time_us as u64 * 1000),
            dts_ns: None,
            data: vec![7; bytes],
        }
    }

    fn memory_only(bytes: usize) -> Budget {
        Budget {
            memory_bytes: bytes,
            disk_bytes: 0,
            dir: None,
        }
    }

    /// 1 s of 10 fps from `start_us`, a keyframe first.
    fn second(outbox: &mut Outbox, start_us: i64, bytes: usize) {
        for i in 0..10 {
            outbox.push(unit(start_us + i * 100_000, i == 0, bytes));
        }
    }

    #[test]
    fn confirmed_units_are_forgotten() {
        let mut outbox = Outbox::new(MediaKind::Video, 1, memory_only(1 << 20));
        second(&mut outbox, 0, 100);
        assert_eq!(outbox.counts().units, 10);
        outbox.confirmed(&SeqRanges::from_wire(&[SeqRange(1, 4), SeqRange(9, 20)]));
        assert_eq!(
            outbox.counts(),
            Counts {
                units: 4,
                memory_bytes: 400,
                disk_bytes: 0,
                oldest_us: Some(400_000)
            }
        );
    }

    #[test]
    fn live_starts_at_a_keyframe_and_the_rest_is_backlog() {
        let mut outbox = Outbox::new(MediaKind::Video, 1, memory_only(1 << 20));
        second(&mut outbox, 0, 100); // 1–10: before the connection
        outbox.connected();
        outbox.push(unit(1_000_000, false, 100)); // 11: mid-group, still backlog
        assert!(
            outbox.next_live(None).unwrap().is_none(),
            "waits for a keyframe"
        );
        second(&mut outbox, 1_100_000, 100); // 12–21, keyframe at 12
        let (first, gop) = outbox.next_live(None).unwrap().unwrap();
        assert_eq!((first.seq, gop), (12, 1_100_000));
        outbox.mark_sent(12);
        assert_eq!(outbox.next_live(Some(12)).unwrap().unwrap().0.seq, 13);
        // The backlog is 1–11, oldest first; 11 belongs to the group from 0.
        let (backlog, gop) = outbox.next_backlog().unwrap().unwrap();
        assert_eq!((backlog.seq, gop), (1, 0));
        for seq in 1..=10 {
            outbox.mark_sent(seq);
        }
        let (last, gop) = outbox.next_backlog().unwrap().unwrap();
        assert_eq!((last.seq, gop), (11, 0));
        outbox.mark_sent(11);
        assert!(outbox.next_backlog().unwrap().is_none());
        // A lost connection sends everything unconfirmed again.
        outbox.disconnected();
        outbox.connected();
        assert_eq!(outbox.next_backlog().unwrap().unwrap().0.seq, 1);
    }

    #[test]
    fn a_live_sender_far_behind_jumps_to_the_newest_keyframe() {
        let mut outbox = Outbox::new(MediaKind::Video, 1, memory_only(1 << 20));
        outbox.connected();
        for s in 0..8 {
            second(&mut outbox, s * 1_000_000, 100);
        }
        // Nothing sent yet and 7 s behind: live jumps to the keyframe at 7 s.
        let (next, _) = outbox.next_live(None).unwrap().unwrap();
        assert_eq!((next.seq, next.time_us), (71, 7_000_000));
        // What it skipped is the backlog now.
        assert_eq!(outbox.next_backlog().unwrap().unwrap().0.seq, 1);
    }

    #[test]
    fn over_the_memory_budget_units_go_to_disk_and_come_back() {
        let dir = tempfile::tempdir().unwrap();
        let budget = Budget {
            memory_bytes: 2_500,
            disk_bytes: 1 << 20,
            dir: Some(dir.path().to_owned()),
        };
        let mut outbox = Outbox::new(MediaKind::Video, 0xab, budget);
        second(&mut outbox, 0, 1000);
        let counts = outbox.counts();
        assert_eq!(
            (counts.units, counts.memory_bytes, counts.disk_bytes),
            (10, 2000, 8000)
        );
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
        outbox.connected();
        let (back, _) = outbox.next_backlog().unwrap().unwrap();
        assert_eq!(
            (back.seq, back.data.len()),
            (1, 1000),
            "read back from disk"
        );
        assert!(back.data.iter().all(|&b| b == 7));
        // Confirming everything on disk deletes the file once it is not the one written.
        outbox.confirmed(&SeqRanges::from_wire(&[SeqRange(1, 10)]));
        assert_eq!(outbox.counts(), Counts::default());
        drop(outbox);
        assert_eq!(
            fs::read_dir(dir.path()).unwrap().count(),
            0,
            "dropped tracks leave no files"
        );
    }

    #[test]
    fn a_full_folder_gives_up_the_oldest() {
        let dir = tempfile::tempdir().unwrap();
        let budget = Budget {
            memory_bytes: 1_000,
            disk_bytes: 3_000,
            dir: Some(dir.path().to_owned()),
        };
        let mut outbox = Outbox::new(MediaKind::Video, 1, budget);
        second(&mut outbox, 0, 1000);
        let counts = outbox.counts();
        assert_eq!(
            (counts.units, counts.disk_bytes),
            (4, 3000),
            "3 on disk, 1 in memory"
        );
        assert_eq!(outbox.counts().oldest_us, Some(600_000));
        assert_eq!(
            outbox.take_given_up(),
            [GivenUp {
                seqs: SeqRange(1, 6),
                from_time_us: 0,
                to_time_us: 500_000,
                reason: GapReason::OutageFull
            }]
        );
        // No folder at all: over memory is given up at once.
        let mut bare = Outbox::new(MediaKind::Video, 2, memory_only(1_500));
        second(&mut bare, 0, 1000);
        assert_eq!(bare.counts().units, 1);
    }

    #[test]
    fn a_clean_stop_keeps_units_for_the_next_run() {
        let dir = tempfile::tempdir().unwrap();
        let budget = Budget {
            memory_bytes: 2_500,
            disk_bytes: 1 << 20,
            dir: Some(dir.path().to_owned()),
        };
        let mut outbox = Outbox::new(MediaKind::Video, 0x77, budget.clone());
        second(&mut outbox, 0, 1000); // 1–10; 1–8 on disk, 9–10 in memory
        outbox.confirmed(&SeqRanges::from_wire(&[SeqRange(1, 2)]));
        outbox.persist();
        drop(outbox);
        // The next run finds 3–10, numbered as before, all backlog.
        let mut back = Outbox::recover(MediaKind::Video, 0x77, budget).unwrap();
        assert_eq!(back.counts().units, 8);
        assert_eq!(back.counts().memory_bytes, 0);
        back.connected();
        let (first, gop) = back.next_backlog().unwrap().unwrap();
        assert_eq!((first.seq, first.data.len(), gop), (3, 1000, 0));
        assert!(back.next_live(None).unwrap().is_none());
        // New units continue the numbering.
        assert_eq!(back.push(unit(1_000_000, true, 10)), 11);
        back.confirmed(&SeqRanges::from_wire(&[SeqRange(1, 11)]));
        assert_eq!(back.counts().units, 0);
        drop(back);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn units_older_than_the_window_are_given_up() {
        let mut outbox = Outbox::new(MediaKind::Video, 1, memory_only(1 << 20));
        second(&mut outbox, 0, 10);
        second(&mut outbox, 1_000_000, 10);
        outbox.expire(1_000_000);
        assert_eq!(outbox.counts().units, 10);
        let gaps = outbox.take_given_up();
        assert_eq!(gaps.len(), 1);
        assert_eq!(
            (gaps[0].seqs, gaps[0].reason),
            (SeqRange(1, 10), GapReason::Expired)
        );
        assert!(outbox.take_given_up().is_empty());
    }
}
