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
                        after.is_none_or(|a| unit.seq != a + 1)
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
                        last.is_none_or(|l| unit.seq != l + 1)
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

    // ------------------------------------------------------------------
    // Sessions against a scripted stand-in ingest (TLS over TCP, loopback)
    // ------------------------------------------------------------------

    use crate::{tls_config, AssignFuture, Backup, Config, Status};
    use ed25519_dalek::SigningKey;
    use nw_backup_protocol::{
        frame::{self, flags, MediaUnit},
        messages::{
            Ack, Codec, Move, Refused, TicketAccepted, TrackFormat, TrackKind, TrackOpened,
            TrackRefusal, TrackRefused, Welcome,
        },
        ranges::SeqRange,
        ALPN,
    };
    use std::sync::{atomic::AtomicUsize, Mutex};
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// What the stand-in does after a message from the recorder.
    enum Reply {
        Send(Vec<ServerMessage>),
        /// Garbage where a control message belongs.
        Garbage,
        /// Hang up.
        Close,
        Nothing,
    }

    /// `(connection number, message)` to what to do.
    type Script = Arc<dyn Fn(usize, &ClientMessage) -> Reply + Send + Sync>;

    #[derive(Default)]
    struct Seen {
        names: Mutex<Vec<String>>,
        connections: AtomicUsize,
    }

    impl Seen {
        fn count(&self, name: &str) -> usize {
            self.names
                .lock()
                .unwrap()
                .iter()
                .filter(|n| *n == name)
                .count()
        }
    }

    fn welcome() -> ServerMessage {
        ServerMessage::Welcome(Welcome {
            session_id: "s".into(),
            server_time_ms: 1_790_000_000_000,
            ack_interval_ms: 100,
            max_tracks: 8,
            instance_proof: None,
        })
    }

    fn opened(track: u32) -> ServerMessage {
        ServerMessage::TrackOpened(TrackOpened {
            track,
            durable: vec![],
        })
    }

    fn base64url(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let n = chunk
                .iter()
                .enumerate()
                .fold(0u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
            for i in 0..=chunk.len() {
                out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
            }
        }
        out
    }

    fn now_ms() -> i64 {
        now_us() / 1000
    }

    /// A ticket naming `cam-1` (the stand-in does not check signatures).
    fn ticket_text(life_ms: i64, retention_s: u32) -> String {
        let claims = serde_json::json!({
            "v": 1, "id": "t", "org_id": "org", "node_id": "node",
            "node_key": "00".repeat(32), "audience": "localhost",
            "issued_at_ms": now_ms(), "expires_at_ms": now_ms() + life_ms,
            "cameras": [{"camera_id": "cam-1", "retention_s": retention_s,
                         "limits": {"max_long_side": 1920, "max_short_side": 1080,
                                    "max_fps": 30.0, "max_bitrate_bps": 4000000}}]
        });
        format!(
            "{}.{}",
            base64url(claims.to_string().as_bytes()),
            "00".repeat(64)
        )
    }

    fn h264() -> TrackFormat {
        TrackFormat {
            codec: Codec::H264,
            width: Some(1920),
            height: Some(1080),
            sample_rate: None,
            channels: None,
            codec_data: None,
        }
    }

    fn unit(seq: u64, time_us: i64) -> MediaUnit {
        MediaUnit {
            media: MediaKind::Video,
            flags: if seq == 1 { flags::KEYFRAME } else { 0 },
            track: 0,
            seq,
            time_us,
            pts_ns: Some(seq * 40_000_000),
            dts_ns: None,
            data: vec![0, 0, 0, 1, 0x65, seq as u8],
        }
    }

    async fn read_frame(input: &mut (impl AsyncRead + Unpin)) -> Option<Frame> {
        let mut prefix = [0u8; frame::PREFIX_BYTES];
        input.read_exact(&mut prefix).await.ok()?;
        let mut body = vec![0u8; frame::body_length(prefix).ok()?];
        input.read_exact(&mut body).await.ok()?;
        frame::decode_body(&body).ok()
    }

    async fn ingest(script: Script) -> (u16, Vec<u8>, Arc<Seen>) {
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
        let cert = certified.cert.der().to_vec();
        let key =
            rustls::pki_types::PrivateKeyDer::Pkcs8(certified.key_pair.serialize_der().into());
        let mut server = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert.clone().into()], key)
        .unwrap();
        server.alpn_protocols = vec![ALPN.to_vec()];
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Seen::default());
        let shared = Arc::clone(&seen);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let Ok(tls) = acceptor.accept(stream).await else {
                    continue;
                };
                let connection = shared.connections.fetch_add(1, Ordering::SeqCst);
                let (script, seen) = (Arc::clone(&script), Arc::clone(&shared));
                tokio::spawn(async move {
                    let (mut input, mut output) = tokio::io::split(tls);
                    while let Some(frame) = read_frame(&mut input).await {
                        let Frame::Message(json) = frame else {
                            continue;
                        };
                        let Ok(message) = messages::decode::<ClientMessage>(&json) else {
                            continue;
                        };
                        let name = serde_json::to_value(&message).unwrap()["type"]
                            .as_str()
                            .unwrap_or("?")
                            .to_owned();
                        seen.names.lock().unwrap().push(name);
                        let mut bytes = Vec::new();
                        match script(connection, &message) {
                            Reply::Send(replies) => {
                                for reply in replies {
                                    frame::encode_message(&mut bytes, &messages::encode(&reply))
                                        .unwrap();
                                }
                            }
                            Reply::Garbage => {
                                frame::encode_message(&mut bytes, b"{this is not json").unwrap();
                            }
                            Reply::Close => return,
                            Reply::Nothing => continue,
                        }
                        if output.write_all(&bytes).await.is_err() || output.flush().await.is_err()
                        {
                            return;
                        }
                    }
                });
            }
        });
        (port, cert, seen)
    }

    fn script(f: impl Fn(usize, &ClientMessage) -> Reply + Send + Sync + 'static) -> Script {
        Arc::new(f)
    }

    /// A script that welcomes and opens tracks, and lets `then` decide the rest.
    fn friendly(
        then: impl Fn(usize, &ClientMessage) -> Option<Reply> + Send + Sync + 'static,
    ) -> Script {
        Arc::new(move |connection, message| {
            if let Some(reply) = then(connection, message) {
                return reply;
            }
            match message {
                ClientMessage::Hello(_) => Reply::Send(vec![welcome()]),
                ClientMessage::OpenTrack(open) => Reply::Send(vec![opened(open.track)]),
                ClientMessage::Ping(ping) => Reply::Send(vec![ServerMessage::Pong(ping.clone())]),
                _ => Reply::Nothing,
            }
        })
    }

    struct Portal {
        calls: AtomicUsize,
        answer: Box<dyn Fn(usize) -> Result<crate::Assignment, String> + Send + Sync>,
    }

    impl crate::Assigner for Portal {
        fn assign(&self) -> AssignFuture<'_> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            let answer = (self.answer)(call);
            Box::pin(async move { answer })
        }
    }

    fn portal(
        answer: impl Fn(usize) -> Result<crate::Assignment, String> + Send + Sync + 'static,
    ) -> Arc<Portal> {
        Arc::new(Portal {
            calls: AtomicUsize::new(0),
            answer: Box::new(answer),
        })
    }

    fn assignment(port: u16, ticket: String) -> crate::Assignment {
        crate::Assignment {
            ticket,
            server_name: "localhost".into(),
            quic: None,
            tcp: Some(format!("127.0.0.1:{port}")),
            instance_key: None,
        }
    }

    fn fixed(port: u16, ticket: impl Fn() -> String + Send + Sync + 'static) -> Arc<Portal> {
        portal(move |_| Ok(assignment(port, ticket())))
    }

    fn start(ca: &[u8], portal: Arc<Portal>) -> Backup {
        Backup::start(
            Config {
                node_key: SigningKey::from_bytes(&[7; 32]),
                recorder: "test".into(),
                tls: tls_config(Some(ca)).unwrap(),
                outage_dir: None,
                memory_bytes_per_track: 1 << 20,
                disk_bytes_per_track: 0,
            },
            portal,
        )
    }

    async fn until(what: &str, mut done: impl FnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(20), async {
            while !done() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
    }

    async fn retrying_with(backup: &Backup, text: &str) {
        until(text, || {
            let status = backup.status();
            status.state == "retrying"
                && status.problem.as_deref().is_some_and(|p| p.contains(text))
        })
        .await;
    }

    #[test]
    fn refusals_are_explained_and_the_ingests_identity_is_checked() {
        assert!(refusal_text(RefusalCode::TicketExpired, "x").contains("expired"));
        assert!(refusal_text(RefusalCode::Busy, "x").contains("busy"));
        assert_eq!(
            refusal_text(RefusalCode::BadTicket, "no plan"),
            "The backup server refused the connection: no plan"
        );
        let key = SigningKey::from_bytes(&[3; 32]);
        let named = |key: Option<String>| crate::Assignment {
            ticket: "ticket".into(),
            server_name: "localhost".into(),
            quic: None,
            tcp: None,
            instance_key: key,
        };
        let binding = [9u8; 32];
        let proof = nw_service_protocol::sign_hex(
            &key,
            &nw_service_protocol::instance_proof_payload("ticket", &binding),
        );
        let public = nw_service_protocol::hex(key.verifying_key().as_bytes());
        // No key named: the certificate alone identifies the ingest.
        assert!(proves_instance(&named(None), &binding, None));
        assert!(proves_instance(
            &named(Some(public.clone())),
            &binding,
            Some(&proof)
        ));
        assert!(!proves_instance(
            &named(Some(public.clone())),
            &binding,
            None
        ));
        assert!(!proves_instance(
            &named(Some(public.clone())),
            &[1u8; 32],
            Some(&proof)
        ));
        assert!(!proves_instance(
            &named(Some("not hex".into())),
            &binding,
            Some(&proof)
        ));
    }

    #[tokio::test]
    async fn problems_before_a_connection_are_reported_in_plain_words() {
        let (port, ca, _) = ingest(friendly(|_, _| None)).await;
        // The portal gives no ticket.
        let backup = start(&ca, portal(|_| Err("portal down".into())));
        retrying_with(&backup, "No backup ticket: portal down").await;
        backup.stop().await;
        // A ticket that cannot be read.
        let backup = start(&ca, fixed(port, || "garbage".into()));
        retrying_with(&backup, "could not be read").await;
        backup.stop().await;
        // No address to connect to.
        let backup = start(
            &ca,
            portal(|_| {
                Ok(crate::Assignment {
                    tcp: None,
                    ..assignment(1, ticket_text(900_000, 3600))
                })
            }),
        );
        retrying_with(&backup, "could not be reached").await;
        backup.stop().await;
        // Nothing listens at the address.
        let closed = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let backup = start(&ca, fixed(closed, || ticket_text(900_000, 3600)));
        retrying_with(&backup, "TCP:").await;
        backup.stop().await;
    }

    #[tokio::test]
    async fn a_hello_that_is_refused_ignored_or_answered_with_nonsense_fails_the_session() {
        let ticket = || ticket_text(900_000, 3600);
        // Hung up on, then answered with garbage, then refused as busy.
        for (script, expected) in [
            (script(|_, _| Reply::Close), "did not answer"),
            (script(|_, _| Reply::Garbage), "did not answer"),
            (
                script(|_, _| {
                    Reply::Send(vec![ServerMessage::Refused(Refused {
                        code: RefusalCode::Busy,
                        message: "full".into(),
                        retry_after_ms: None,
                    })])
                }),
                "busy",
            ),
            // A welcome that does not prove the ingest's identity.
            (
                script(|_, _| Reply::Send(vec![welcome()])),
                "prove who it is",
            ),
        ] {
            let (port, ca, _) = ingest(script).await;
            let backup = start(
                &ca,
                portal(move |_| {
                    let mut named = assignment(port, ticket());
                    if expected == "prove who it is" {
                        named.instance_key = Some(nw_service_protocol::hex(
                            SigningKey::from_bytes(&[5; 32]).verifying_key().as_bytes(),
                        ));
                    }
                    Ok(named)
                }),
            );
            retrying_with(&backup, expected).await;
            backup.stop().await;
        }
    }

    #[tokio::test]
    async fn a_track_the_plan_refuses_says_why_and_whether_a_smaller_stream_may_fit() {
        let script = friendly(|_, message| match message {
            ClientMessage::OpenTrack(open) => Some(Reply::Send(vec![ServerMessage::TrackRefused(
                TrackRefused {
                    track: open.track,
                    code: TrackRefusal::OverBitrate,
                    message: "too much data".into(),
                    retry_after_ms: Some(60_000),
                },
            )])),
            // Messages that need no action of the recorder.
            ClientMessage::Ping(_) => Some(Reply::Send(vec![
                ServerMessage::TicketAccepted(TicketAccepted { expires_at_ms: 0 }),
                ServerMessage::Ack(Ack {
                    track: 999,
                    durable: vec![SeqRange(1, 2)],
                }),
                ServerMessage::Unknown,
            ])),
            _ => None,
        });
        let (port, ca, seen) = ingest(script).await;
        let backup = start(&ca, fixed(port, || ticket_text(900_000, 3600)));
        let track = backup.track("cam-1", TrackKind::Video, h264());
        until("the refusal", || track.over_plan()).await;
        assert_eq!(track.refusal().as_deref(), Some("too much data"));
        assert_eq!(backup.status().state, "connected");
        // Not offered again within the retry time.
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert_eq!(seen.count("open_track"), 1);
        backup.stop().await;
    }

    #[tokio::test]
    async fn an_ingest_that_moves_the_recorder_gets_a_new_session() {
        let script = friendly(|connection, message| match message {
            ClientMessage::OpenTrack(_) if connection == 0 => {
                Some(Reply::Send(vec![ServerMessage::Move(Move {
                    reason: "draining".into(),
                    retry_after_ms: 10,
                })]))
            }
            _ => None,
        });
        let (port, ca, seen) = ingest(script).await;
        let backup = start(&ca, fixed(port, || ticket_text(900_000, 3600)));
        let _track = backup.track("cam-1", TrackKind::Video, h264());
        until("connected again", || {
            backup.status().state == "connected" && seen.count("open_track") >= 2
        })
        .await;
        assert_eq!(backup.status().problem, None);
        backup.stop().await;
    }

    #[tokio::test]
    async fn a_session_ended_by_the_ingest_mid_way_is_retried() {
        let script = friendly(|connection, message| match message {
            ClientMessage::OpenTrack(open) if connection == 0 => {
                // Opens the track, then a refusal ends the session.
                Some(Reply::Send(vec![
                    opened(open.track),
                    ServerMessage::Refused(Refused {
                        code: RefusalCode::TicketExpired,
                        message: "old".into(),
                        retry_after_ms: None,
                    }),
                ]))
            }
            _ => None,
        });
        let (port, ca, seen) = ingest(script).await;
        let backup = start(&ca, fixed(port, || ticket_text(900_000, 3600)));
        let _track = backup.track("cam-1", TrackKind::Video, h264());
        retrying_with(&backup, "expired").await;
        until("the next session", || seen.count("open_track") >= 2).await;
        backup.stop().await;

        // A hang-up after opening a track is "the connection was lost".
        let script = friendly(|_, message| match message {
            ClientMessage::OpenTrack(open) => Some(Reply::Send(vec![opened(open.track)])),
            ClientMessage::Ping(_) => Some(Reply::Close),
            _ => None,
        });
        let (port, ca, _) = ingest(script).await;
        let backup = start(&ca, fixed(port, || ticket_text(900_000, 3600)));
        let _track = backup.track("cam-1", TrackKind::Video, h264());
        retrying_with(&backup, "lost").await;
        backup.stop().await;
    }

    #[tokio::test]
    async fn a_track_that_goes_away_is_closed_and_units_past_the_window_are_reported_as_a_gap() {
        let (port, ca, seen) = ingest(friendly(|_, _| None)).await;
        // Backup window of one second.
        let backup = start(&ca, fixed(port, || ticket_text(900_000, 1)));
        let track = backup.track("cam-1", TrackKind::Video, h264());
        until("the track to open", || seen.count("open_track") == 1).await;
        track.push(unit(1, now_us() - 120_000_000));
        until("the gap report", || seen.count("gap") >= 1).await;
        drop(track);
        until("the track to close", || seen.count("close_track") == 1).await;
        backup.stop().await;
        assert_eq!(seen.count("bye"), 1);
    }

    #[tokio::test]
    async fn the_ticket_is_renewed_before_it_expires_on_the_same_ingest() {
        let script = friendly(|_, message| match message {
            ClientMessage::Renew(_) => Some(Reply::Send(vec![ServerMessage::TicketAccepted(
                TicketAccepted { expires_at_ms: 0 },
            )])),
            _ => None,
        });
        let (port, ca, seen) = ingest(script).await;
        // A ticket that is about to expire: renewal after the shortest wait.
        let portal = fixed(port, || ticket_text(1, 3600));
        let backup = start(&ca, Arc::clone(&portal));
        until("a renewal", || seen.count("renew") >= 1).await;
        assert!(portal.calls.load(Ordering::SeqCst) >= 2);
        assert_eq!(seen.connections.load(Ordering::SeqCst), 1, "no reconnect");
        backup.stop().await;
    }

    #[tokio::test]
    async fn a_renewal_that_names_another_ingest_moves_the_session_and_a_failed_one_is_retried() {
        let (port, ca, seen) = ingest(friendly(|_, _| None)).await;
        let backup = start(
            &ca,
            portal(move |call| {
                let mut named = assignment(port, ticket_text(1, 3600));
                if call == 1 {
                    named.server_name = "elsewhere".into();
                }
                Ok(named)
            }),
        );
        // (A session is established once it has pinged: stopping earlier would
        // go unnoticed until its handshake ends.)
        until("the move", || seen.count("ping") >= 2).await;
        backup.stop().await;

        let (port, ca, seen) = ingest(friendly(|_, _| None)).await;
        let failing = portal(move |call| {
            if call == 1 {
                Err("portal busy".into())
            } else {
                Ok(assignment(port, ticket_text(1, 3600)))
            }
        });
        let backup = start(&ca, Arc::clone(&failing));
        until("the failed renewal", || {
            failing.calls.load(Ordering::SeqCst) >= 2
        })
        .await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            seen.connections.load(Ordering::SeqCst),
            1,
            "the session carries on"
        );
        assert_eq!(backup.status().state, "connected");
        backup.stop().await;
    }

    #[tokio::test]
    async fn the_status_of_a_stopped_backup_keeps_its_last_state() {
        let (port, ca, _) = ingest(friendly(|_, _| None)).await;
        let backup = start(&ca, fixed(port, || ticket_text(900_000, 3600)));
        until("connected", || backup.status().state == "connected").await;
        let status: Status = backup.status();
        assert_eq!(status.ingest.as_deref(), Some("localhost"));
        backup.stop().await;
    }
}
