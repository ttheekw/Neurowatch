//! Sessions with an ingest, one after another until stopped.
use crate::{
    lock, now_us,
    transport::{self, Connected, Link},
    Assigner, Shared, TrackState,
};
use nw_backup_protocol::{
    frame::{Frame, MediaKind},
    messages::{
        self, ClientMessage, Gap, Hello, OpenTrack, Ping, RefusalCode, Renew, ServerMessage,
    },
    ranges::SeqRanges,
    ticket::{self, Ticket},
    PROTOCOL_VERSION,
};
use std::{
    collections::HashMap,
    sync::{atomic::AtomicU64, atomic::Ordering, Arc},
    time::Duration,
};
use tokio::{sync::mpsc, task::JoinSet, time::Instant};

/// Waits between failed sessions.
const BACKOFF: [Duration; 5] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
    Duration::from_secs(30),
];
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// TLS over TCP needs pings (the ingest ends a session silent for 20 s).
const PING_INTERVAL: Duration = Duration::from_secs(5);
/// A refused track is offered again after this (its plan may have changed).
const REFUSAL_RETRY: Duration = Duration::from_secs(300);
/// A sound run is cut into runs this long (every sound unit is a keyframe).
const SOUND_RUN_US: i64 = 2_000_000;

enum Ended {
    Stopped,
    Moved(Duration),
    Failed(String),
}

static RUNS: AtomicU64 = AtomicU64::new(1);

pub(crate) async fn run(shared: Arc<Shared>, assigner: Arc<dyn Assigner>) {
    let mut stopping = shared.stopping.subscribe();
    let mut failures = 0usize;
    loop {
        if *stopping.borrow() {
            return;
        }
        set_status(&shared, "connecting", None, None, None);
        let ended = session(&shared, assigner.as_ref()).await;
        for track in lock(&shared.tracks).values() {
            lock(&track.outbox).disconnected();
        }
        let wait = match ended {
            Ended::Stopped => return,
            Ended::Moved(wait) => {
                failures = 0;
                wait
            }
            Ended::Failed(problem) => {
                tracing::info!(%problem, "cloud backup: not connected");
                let wait = BACKOFF[failures.min(BACKOFF.len() - 1)];
                failures += 1;
                set_status(&shared, "retrying", None, None, Some(problem));
                wait
            }
        };
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = stopping.changed() => return,
        }
    }
}

fn set_status(
    shared: &Shared,
    state: &'static str,
    ingest: Option<String>,
    transport: Option<&'static str>,
    problem: Option<String>,
) {
    let mut status = lock(&shared.status);
    status.state = state;
    status.ingest = ingest;
    status.transport = transport;
    status.problem = problem;
}

/// One session: assignment, connection, hello, tracks, until it ends.
async fn session(shared: &Arc<Shared>, assigner: &dyn Assigner) -> Ended {
    let assignment = match assigner.assign().await {
        Ok(assignment) => assignment,
        Err(error) => return Ended::Failed(format!("No backup ticket: {error}")),
    };
    let Some(mut claims) = ticket::read_claims(&assignment.ticket) else {
        return Ended::Failed("The backup ticket could not be read.".into());
    };
    let tls = shared.config.tls.clone();
    let mut problems = Vec::new();
    let mut connected: Option<Connected> = None;
    if let Some(address) = &assignment.quic {
        match transport::quic(address, &assignment.server_name, tls.clone()).await {
            Ok(c) => connected = Some(c),
            Err(error) => problems.push(format!("QUIC: {error}")),
        }
    }
    if connected.is_none() {
        if let Some(address) = &assignment.tcp {
            match transport::tcp(address, &assignment.server_name, tls).await {
                Ok(c) => connected = Some(c),
                Err(error) => problems.push(format!("TCP: {error}")),
            }
        }
    }
    let Some(Connected {
        link,
        mut reader,
        binding,
    }) = connected
    else {
        return Ended::Failed(format!(
            "The backup server could not be reached ({}).",
            problems.join("; ")
        ));
    };
    let hello = ClientMessage::Hello(Hello {
        protocol: PROTOCOL_VERSION,
        ticket: assignment.ticket.clone(),
        signature: ticket::sign(
            &shared.config.node_key,
            &ticket::hello_payload(&assignment.ticket, &binding),
        ),
        recorder: shared.config.recorder.clone(),
        clock_offset_ms: 0,
    });
    if let Err(error) = link.send(&hello).await {
        return Ended::Failed(format!("The backup server closed the connection: {error}"));
    }
    match tokio::time::timeout(HELLO_TIMEOUT, next_message(&mut reader)).await {
        Ok(Ok(Some(ServerMessage::Welcome(welcome)))) => {
            if !proves_instance(&assignment, &binding, welcome.instance_proof.as_deref()) {
                link.close();
                return Ended::Failed("The backup server could not prove who it is.".into());
            }
        }
        Ok(Ok(Some(ServerMessage::Refused(refused)))) => {
            link.close();
            return Ended::Failed(refusal_text(refused.code, &refused.message));
        }
        Ok(Ok(_)) | Ok(Err(_)) | Err(_) => {
            link.close();
            return Ended::Failed("The backup server did not answer.".into());
        }
    }
    set_status(
        shared,
        "connected",
        Some(assignment.server_name.clone()),
        Some(link.transport()),
        None,
    );
    tracing::info!(ingest = %assignment.server_name, transport = link.transport(), "cloud backup: connected");

    let (messages_tx, mut messages_rx) = mpsc::channel(256);
    let reading = tokio::spawn(async move {
        while let Ok(Some(message)) = next_message(&mut reader).await {
            if messages_tx.send(message).await.is_err() {
                return;
            }
        }
    });
    let ended = connected_session(
        shared,
        assigner,
        &link,
        &assignment,
        &mut claims,
        &mut messages_rx,
        binding,
    )
    .await;
    reading.abort();
    link.close();
    ended
}

/// Per open track: its senders.
struct Open {
    senders: JoinSet<()>,
}

#[allow(clippy::too_many_arguments)]
async fn connected_session(
    shared: &Arc<Shared>,
    assigner: &dyn Assigner,
    link: &Link,
    assignment: &crate::Assignment,
    claims: &mut Ticket,
    messages: &mut mpsc::Receiver<ServerMessage>,
    binding: [u8; nw_backup_protocol::CHANNEL_BINDING_BYTES],
) -> Ended {
    let mut stopping = shared.stopping.subscribe();
    let mut open: HashMap<u32, Open> = HashMap::new();
    // Tracks asked for and not answered, and refused ones with when to retry.
    let mut asked: HashMap<u32, ()> = HashMap::new();
    let mut retry_at: HashMap<u32, Instant> = HashMap::new();
    let mut ping = tokio::time::interval(PING_INTERVAL);
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut renew_at = renewal_instant(claims);
    let mut renewing = false;
    loop {
        // Offer tracks this session has not opened.
        let tracks: Vec<Arc<TrackState>> = lock(&shared.tracks).values().cloned().collect();
        for track in &tracks {
            let number = track.number;
            if open.contains_key(&number) || asked.contains_key(&number) {
                continue;
            }
            if retry_at.get(&number).is_some_and(|at| *at > Instant::now()) {
                continue;
            }
            if claims.camera(&track.camera_id).is_none() {
                if track.recovered {
                    // Left by a previous run for a camera whose plan has ended.
                    lock(&track.outbox).discard();
                    lock(&shared.tracks).remove(&number);
                    continue;
                }
                *lock(&track.refused) = Some((None, "This camera has no backup plan.".into()));
                retry_at.insert(number, Instant::now() + REFUSAL_RETRY);
                continue;
            }
            let format = lock(&track.format).clone();
            let message = ClientMessage::OpenTrack(OpenTrack {
                track: number,
                camera_id: track.camera_id.clone(),
                kind: track.kind,
                epoch: lock(&track.outbox).epoch,
                format,
            });
            if let Err(error) = link.send(&message).await {
                return Ended::Failed(format!("The connection was lost: {error}"));
            }
            asked.insert(number, ());
        }
        // Close tracks that went away.
        let gone: Vec<u32> = open
            .keys()
            .copied()
            .filter(|n| !tracks.iter().any(|t| t.number == *n))
            .collect();
        for number in gone {
            open.remove(&number);
            let _ = link
                .send(&ClientMessage::CloseTrack(messages::CloseTrack {
                    track: number,
                }))
                .await;
        }

        tokio::select! {
            message = messages.recv() => {
                let Some(message) = message else {
                    return Ended::Failed("The connection to the backup server was lost.".into());
                };
                match message {
                    ServerMessage::TrackOpened(opened) => {
                        asked.remove(&opened.track);
                        if let Some(track) = tracks.iter().find(|t| t.number == opened.track) {
                            {
                                let mut outbox = lock(&track.outbox);
                                outbox.confirmed(&SeqRanges::from_wire(&opened.durable));
                                outbox.connected();
                            }
                            *lock(&track.refused) = None;
                            let mut senders = JoinSet::new();
                            senders.spawn(send_live(track.clone(), link.clone()));
                            senders.spawn(send_backlog(track.clone(), link.clone()));
                            open.insert(opened.track, Open { senders });
                        }
                    }
                    ServerMessage::TrackRefused(refused) => {
                        asked.remove(&refused.track);
                        open.remove(&refused.track);
                        if let Some(track) = tracks.iter().find(|t| t.number == refused.track) {
                            tracing::info!(camera = %track.camera_id, message = %refused.message, "cloud backup: track refused");
                            *lock(&track.refused) = Some((Some(refused.code), refused.message.clone()));
                        }
                        let wait = refused.retry_after_ms.map_or(REFUSAL_RETRY, |ms| Duration::from_millis(u64::from(ms)));
                        retry_at.insert(refused.track, Instant::now() + wait);
                    }
                    ServerMessage::Ack(ack) => {
                        if let Some(track) = tracks.iter().find(|t| t.number == ack.track) {
                            lock(&track.outbox).confirmed(&SeqRanges::from_wire(&ack.durable));
                        }
                    }
                    ServerMessage::Move(moving) => {
                        tracing::info!(reason = %moving.reason, "cloud backup: asked to move");
                        return Ended::Moved(Duration::from_millis(u64::from(moving.retry_after_ms)));
                    }
                    ServerMessage::Refused(refused) => {
                        return Ended::Failed(refusal_text(refused.code, &refused.message));
                    }
                    ServerMessage::TicketAccepted(_) => renewing = false,
                    ServerMessage::Welcome(_) | ServerMessage::Pong(_) | ServerMessage::Unknown => {}
                }
            }
            _ = shared.tracks_changed.notified() => {}
            _ = ping.tick() => {
                if link.send(&ClientMessage::Ping(Ping { at_ms: now_us() / 1000 })).await.is_err() {
                    return Ended::Failed("The connection to the backup server was lost.".into());
                }
            }
            _ = tick.tick() => {
                // Give up what is older than the window; report gaps.
                let now = now_us();
                for track in &tracks {
                    let Some(grant) = claims.camera(&track.camera_id) else { continue };
                    let gaps = {
                        let mut outbox = lock(&track.outbox);
                        outbox.expire(now - i64::from(grant.retention_s) * 1_000_000);
                        outbox.take_given_up()
                    };
                    if !open.contains_key(&track.number) {
                        continue;
                    }
                    for gap in gaps {
                        let message = ClientMessage::Gap(Gap {
                            track: track.number,
                            seqs: Some(gap.seqs),
                            from_time_us: gap.from_time_us,
                            to_time_us: gap.to_time_us,
                            reason: gap.reason,
                        });
                        let _ = link.send(&message).await;
                    }
                }
                // Recovered tracks sent (or expired) entirely are done.
                for track in &tracks {
                    if track.recovered && open.contains_key(&track.number) && lock(&track.outbox).counts().units == 0 {
                        open.remove(&track.number);
                        lock(&track.outbox).discard();
                        lock(&shared.tracks).remove(&track.number);
                        let _ = link.send(&ClientMessage::CloseTrack(messages::CloseTrack { track: track.number })).await;
                    }
                }
                // A sender failing means the connection is going.
                for open in open.values_mut() {
                    if let Some(Ok(())) = open.senders.try_join_next() {
                        return Ended::Failed("The connection to the backup server was lost.".into());
                    }
                }
            }
            _ = tokio::time::sleep_until(renew_at), if !renewing => {
                renewing = true;
                match assigner.assign().await {
                    Ok(next) if next.server_name == assignment.server_name => {
                        let Some(next_claims) = ticket::read_claims(&next.ticket) else {
                            return Ended::Failed("The backup ticket could not be read.".into());
                        };
                        let renew = ClientMessage::Renew(Renew {
                            signature: ticket::sign(&shared.config.node_key, &ticket::renew_payload(&next.ticket, &binding)),
                            ticket: next.ticket,
                        });
                        if link.send(&renew).await.is_err() {
                            return Ended::Failed("The connection to the backup server was lost.".into());
                        }
                        *claims = next_claims;
                        renew_at = renewal_instant(claims);
                    }
                    // Assigned elsewhere: go there.
                    Ok(_) => return Ended::Moved(Duration::ZERO),
                    Err(error) => {
                        tracing::warn!(%error, "cloud backup: ticket renewal failed; retrying");
                        renewing = false;
                        renew_at = Instant::now() + Duration::from_secs(30);
                    }
                }
            }
            _ = stopping.changed() => {
                let _ = link.send(&ClientMessage::Bye).await;
                // Let the goodbye arrive before the connection closes.
                tokio::time::sleep(Duration::from_millis(200)).await;
                return Ended::Stopped;
            }
        }
    }
}

/// Renew at two thirds of the ticket's life, and within a day whatever its
/// times say: a ticket with absurd times must not overflow (a panic in debug
/// builds, a wrapped deadline in release ones).
fn renewal_instant(claims: &Ticket) -> Instant {
    const MOST_MS: i64 = 24 * 60 * 60 * 1000;
    let life = claims
        .expires_at_ms
        .saturating_sub(claims.issued_at_ms)
        .clamp(0, i64::MAX / 2);
    let at = claims.issued_at_ms.saturating_add(life / 3 * 2);
    let left = at.saturating_sub(now_us() / 1000).clamp(1_000, MOST_MS);
    Instant::now() + Duration::from_millis(left as u64)
}

/// Live units, as they are pushed, from the next keyframe on.
async fn send_live(track: Arc<TrackState>, link: Link) {
    let mut pushed = track.pushed.subscribe();
    let mut after: Option<u64> = None;
    let mut run: Option<(transport::RunOut, i64)> = None;
    loop {
        let next = lock(&track.outbox).next_live(after);
        match next {
            Ok(Some((mut unit, gop_us))) => {
                // This session's number for the track: units recovered after a
                // restart were numbered under the track they had then.
                unit.track = track.number;
                let new_run = match &run {
                    None => true,
                    Some((_, started)) => {
                        after.map_or(true, |a| unit.seq != a + 1)
                            || (unit.media == MediaKind::Video && unit.keyframe())
                            || (unit.media == MediaKind::Audio
                                && unit.time_us - started > SOUND_RUN_US)
                    }
                };
                if new_run {
                    if let Some((old, _)) = run.take() {
                        old.finish();
                    }
                    match link
                        .run(track.number, true, RUNS.fetch_add(1, Ordering::Relaxed))
                        .await
                    {
                        Ok(out) => run = Some((out, unit.time_us)),
                        Err(_) => return,
                    }
                }
                if let Some((out, _)) = run.as_mut() {
                    if out.send(&unit, gop_us).await.is_err() {
                        return;
                    }
                }
                lock(&track.outbox).mark_sent(unit.seq);
                after = Some(unit.seq);
            }
            Ok(None) => {
                if pushed.changed().await.is_err() {
                    return;
                }
            }
            Err(error) => {
                // The outage folder failed: end the session; the next one
                // starts over from what is confirmed.
                tracing::warn!(%error, "cloud backup: a unit could not be read back");
                return;
            }
        }
    }
}

/// The backlog, oldest first, a picture group per run, at a lower priority.
async fn send_backlog(track: Arc<TrackState>, link: Link) {
    let mut last: Option<u64> = None;
    let mut run: Option<(transport::RunOut, i64)> = None;
    loop {
        let next = lock(&track.outbox).next_backlog();
        match next {
            Ok(Some((mut unit, gop_us))) => {
                unit.track = track.number;
                let new_run = match &run {
                    None => true,
                    Some((_, started)) => {
                        last.map_or(true, |l| unit.seq != l + 1)
                            || (unit.media == MediaKind::Video && unit.keyframe())
                            || (unit.media == MediaKind::Audio
                                && unit.time_us - started > SOUND_RUN_US)
                    }
                };
                if new_run {
                    if let Some((old, _)) = run.take() {
                        old.finish();
                    }
                    match link
                        .run(track.number, false, RUNS.fetch_add(1, Ordering::Relaxed))
                        .await
                    {
                        Ok(out) => run = Some((out, unit.time_us)),
                        Err(_) => return,
                    }
                }
                if let Some((out, _)) = run.as_mut() {
                    if out.send(&unit, gop_us).await.is_err() {
                        return;
                    }
                }
                lock(&track.outbox).mark_sent(unit.seq);
                last = Some(unit.seq);
            }
            Ok(None) => {
                if let Some((old, _)) = run.take() {
                    old.finish();
                }
                last = None;
                // The backlog grows only when live jumps ahead or a session
                // starts: look again shortly.
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            Err(error) => {
                tracing::warn!(%error, "cloud backup: a unit could not be read back");
                return;
            }
        }
    }
}

async fn next_message(reader: &mut transport::Reader) -> std::io::Result<Option<ServerMessage>> {
    loop {
        match transport::read_frame(reader).await? {
            None => return Ok(None),
            Some(Frame::Message(json)) => {
                return messages::decode(&json)
                    .map(Some)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e));
            }
            Some(Frame::Media(_)) => continue,
        }
    }
}

fn refusal_text(code: RefusalCode, message: &str) -> String {
    match code {
        RefusalCode::TicketExpired => "The backup ticket expired; asking for a new one.".into(),
        RefusalCode::Busy => "The backup server is busy; trying again.".into(),
        _ => format!("The backup server refused the connection: {message}"),
    }
}

/// Whether the ingest holds the catalog key the portal named (no key named:
/// its TLS certificate alone identifies it).
fn proves_instance(assignment: &crate::Assignment, binding: &[u8], proof: Option<&str>) -> bool {
    let Some(expected) = assignment.instance_key.as_deref() else {
        return true;
    };
    let (Some(key), Some(proof)) = (nw_service_protocol::verifying_key(expected), proof) else {
        return false;
    };
    nw_service_protocol::verify_hex(
        &key,
        &nw_service_protocol::instance_proof_payload(&assignment.ticket, binding),
        proof,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ticket(issued_at_ms: i64, expires_at_ms: i64) -> Ticket {
        Ticket {
            v: 1,
            id: "t".into(),
            org_id: "o".into(),
            node_id: "n".into(),
            node_key: "00".repeat(32),
            audience: "a".into(),
            issued_at_ms,
            expires_at_ms,
            cameras: vec![],
        }
    }

    #[test]
    fn renewal_comes_at_two_thirds_of_a_tickets_life_and_never_overflows() {
        let now = now_us() / 1000;
        let soon = renewal_instant(&ticket(now, now + 15 * 60 * 1000));
        let left = soon.saturating_duration_since(Instant::now());
        assert!(
            left > Duration::from_secs(9 * 60) && left <= Duration::from_secs(10 * 60),
            "{left:?}"
        );
        // Absurd times: a day at most, at least a second, and no panic.
        for (issued, expires) in [
            (0, i64::MAX),
            (i64::MIN, i64::MAX),
            (i64::MAX, i64::MIN),
            (now, now - 1),
        ] {
            let at = renewal_instant(&ticket(issued, expires));
            let left = at.saturating_duration_since(Instant::now());
            assert!(
                left <= Duration::from_secs(24 * 60 * 60),
                "{issued} {expires}: {left:?}"
            );
        }
    }
}
