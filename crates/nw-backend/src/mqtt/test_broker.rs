//! A small MQTT 3.1.1 broker for tests, so the service is exercised end to
//! end without a real one: connect, subscribe (with `+` and `#`), publish at
//! QoS 0 and 1, retained messages (an empty one clears), ping, and the last
//! will on a connection lost without a goodbye. Built on rumqttc's packet
//! codec; nothing here is used outside tests.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use bytes::BytesMut;
use rumqttc::{
    ConnAck, ConnectReturnCode, Packet, PubAck, Publish, QoS, SubAck, SubscribeReasonCode,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

const MAX_PACKET: usize = 1 << 20;

#[derive(Default)]
struct Shared {
    subscribers: Vec<(Vec<String>, mpsc::UnboundedSender<Publish>)>,
    retained: BTreeMap<String, Publish>,
}

/// Whether `topic` matches the subscription `filter`.
pub(super) fn matches(filter: &str, topic: &str) -> bool {
    let mut levels = topic.split('/');
    for part in filter.split('/') {
        match (part, levels.next()) {
            ("#", _) => return true,
            ("+", Some(_)) => {}
            (part, Some(level)) if part == level => {}
            _ => return false,
        }
    }
    levels.next().is_none()
}

/// Starts the broker on a free local port; it runs until the test ends.
pub(super) async fn start() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("address").port();
    let shared = Arc::new(Mutex::new(Shared::default()));
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(client(stream, Arc::clone(&shared)));
        }
    });
    port
}

fn route(shared: &Mutex<Shared>, publish: &Publish) {
    let mut shared = shared.lock().expect("broker state");
    if publish.retain {
        if publish.payload.is_empty() {
            shared.retained.remove(&publish.topic);
        } else {
            shared
                .retained
                .insert(publish.topic.clone(), publish.clone());
        }
    }
    shared.subscribers.retain(|(filters, tx)| {
        if filters.iter().any(|f| matches(f, &publish.topic)) {
            let mut copy = Publish::new(
                publish.topic.clone(),
                QoS::AtMostOnce,
                publish.payload.to_vec(),
            );
            copy.retain = false;
            return tx.send(copy).is_ok();
        }
        !tx.is_closed()
    });
}

async fn client(mut stream: TcpStream, shared: Arc<Mutex<Shared>>) {
    let (tx, mut outgoing) = mpsc::unbounded_channel::<Publish>();
    let mut input = BytesMut::new();
    let mut will = None;
    let mut goodbye = false;
    loop {
        // Whole packets already read first: one read may carry several.
        while let Ok(packet) = Packet::read(&mut input, MAX_PACKET) {
            let reply = match packet {
                Packet::Connect(connect) => {
                    will = connect.last_will.map(|w| {
                        let mut publish = Publish::new(w.topic, w.qos, w.message.to_vec());
                        publish.retain = w.retain;
                        publish
                    });
                    Some(Packet::ConnAck(ConnAck::new(
                        ConnectReturnCode::Success,
                        false,
                    )))
                }
                Packet::Subscribe(subscribe) => {
                    let filters: Vec<String> =
                        subscribe.filters.iter().map(|f| f.path.clone()).collect();
                    let codes = subscribe
                        .filters
                        .iter()
                        .map(|f| SubscribeReasonCode::Success(f.qos))
                        .collect();
                    let retained: Vec<Publish> = {
                        let mut shared = shared.lock().expect("broker state");
                        shared.subscribers.push((filters.clone(), tx.clone()));
                        shared
                            .retained
                            .values()
                            .filter(|p| filters.iter().any(|f| matches(f, &p.topic)))
                            .cloned()
                            .collect()
                    };
                    for publish in retained {
                        let _ = tx.send(publish);
                    }
                    Some(Packet::SubAck(SubAck::new(subscribe.pkid, codes)))
                }
                Packet::Publish(publish) => {
                    route(&shared, &publish);
                    (publish.qos != QoS::AtMostOnce)
                        .then(|| Packet::PubAck(PubAck::new(publish.pkid)))
                }
                Packet::PingReq => Some(Packet::PingResp),
                Packet::Disconnect => {
                    goodbye = true;
                    None
                }
                _ => None,
            };
            if let Some(reply) = reply {
                let mut out = BytesMut::new();
                if reply.write(&mut out, MAX_PACKET).is_err()
                    || stream.write_all(&out).await.is_err()
                {
                    return;
                }
            }
            if goodbye {
                return;
            }
        }
        let mut chunk = [0u8; 8192];
        tokio::select! {
            read = stream.read(&mut chunk) => match read {
                Ok(0) | Err(_) => break,
                Ok(n) => input.extend_from_slice(&chunk[..n]),
            },
            Some(publish) = outgoing.recv() => {
                let mut out = BytesMut::new();
                if Packet::Publish(publish).write(&mut out, MAX_PACKET).is_err()
                    || stream.write_all(&out).await.is_err()
                {
                    break;
                }
            }
        }
    }
    // Lost without a goodbye: its last will is published.
    if let Some(will) = will {
        route(&shared, &will);
    }
}

#[test]
fn filters_match_as_mqtt_says() {
    assert!(matches("a/#", "a/b/c"));
    assert!(matches("a/+/c", "a/b/c"));
    assert!(matches("a/b", "a/b"));
    assert!(!matches("a/+", "a/b/c"));
    assert!(!matches("a/b", "a/c"));
    assert!(matches("#", "anything/at/all"));
}
