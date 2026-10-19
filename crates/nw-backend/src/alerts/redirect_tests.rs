#[tokio::test]
async fn security_webhooks_do_not_forward_tokens_or_pictures_on_redirects() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    async fn receive(socket: &mut tokio::net::TcpStream) -> String {
        let mut received = Vec::new();
        let mut buffer = [0; 4096];
        loop {
            let read = socket.read(&mut buffer).await.unwrap();
            assert!(read > 0);
            received.extend_from_slice(&buffer[..read]);
            let text = String::from_utf8_lossy(&received);
            if let Some((head, body)) = text.split_once("\r\n\r\n") {
                let length = head
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .map(|value| value.parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                if body.len() >= length {
                    break;
                }
            }
        }
        String::from_utf8(received).unwrap()
    }
    for code in [302, 307, 308] {
        let origin = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let destination = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/hook", origin.local_addr().unwrap());
        let target_url = format!("http://{}/stolen", destination.local_addr().unwrap());
        let source = tokio::spawn(async move {
            let (mut socket, _) = origin.accept().await.unwrap();
            let request = receive(&mut socket).await;
            socket.write_all(format!("HTTP/1.1 {code} Redirect\r\nLocation: {target_url}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
            request
        });
        let sink = tokio::spawn(async move {
            let Ok(Ok((mut socket, _))) =
                tokio::time::timeout(Duration::from_millis(300), destination.accept()).await
            else {
                return None;
            };
            let request = receive(&mut socket).await;
            socket
                .write_all(
                    b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            Some(request)
        });
        let target = ChannelTarget::Webhook {
            url,
            secret: "private-hook-token".into(),
        };
        let alert = alert_for(&event(EventKind::Motion, Some("cam-1")), Some("Gate"), None);
        let result = deliver::deliver(
            AlertService::default().client.as_ref().unwrap(),
            &target,
            &alert,
            Some(b"private-picture"),
        )
        .await;
        let original = source.await.unwrap();
        let leaked = sink.await.unwrap();
        assert!(original.contains("private-hook-token"));
        assert!(original.contains("picture_jpeg_b64"));
        assert_eq!(result, Err(format!("HTTP {code}")));
        assert!(
            leaked.is_none(),
            "HTTP {code} must not forward the token or event"
        );
    }
}

#[tokio::test]
async fn security_an_unavailable_http_client_never_falls_back_to_following_redirects() {
    let service = AlertService {
        client: None,
        ..AlertService::default()
    };
    let channel: AlertChannel = serde_json::from_value(json!({
        "id":"hook", "name":"Hook", "kind":"webhook",
        "url":"http://127.0.0.1:9/hook", "secret":"test-secret"
    }))
    .unwrap();
    assert_eq!(
        service.test(&channel, "Site", false).await,
        Err("HTTP alert client is unavailable".into())
    );
    assert!(
        service
            .test(&config().channels[0], "Site", false)
            .await
            .is_ok(),
        "desktop alerts remain available"
    );
}
