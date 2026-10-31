use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::{Message, WebSocketConfig};
use tokio_tungstenite::{accept_async, connect_async_with_config};

use super::websocket::response_websocket_config;

#[test]
fn production_websocket_receive_limits_remain_at_library_defaults() {
    let config = response_websocket_config();
    assert_eq!(config.max_message_size, Some(64 << 20));
    assert_eq!(config.max_frame_size, Some(16 << 20));
}

#[test]
fn small_test_receive_limit_rejects_oversized_message() {
    crate::runtime::block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind local WebSocket receiver");
        let address = listener.local_addr().expect("local WebSocket address");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept local WebSocket");
            let mut socket = accept_async(stream).await.expect("accept test handshake");
            socket
                .send(Message::text("x".repeat(64)))
                .await
                .expect("send oversized fixture message");
        });
        let request = format!("ws://{address}/responses")
            .into_client_request()
            .expect("construct test WebSocket request");
        let config = WebSocketConfig::default().max_message_size(Some(32));
        let (mut socket, _) = connect_async_with_config(request, Some(config), false)
            .await
            .expect("connect test WebSocket client");
        let received = socket.next().await.expect("receive oversized frame");
        assert!(matches!(
            received,
            Err(tokio_tungstenite::tungstenite::Error::Capacity(_))
        ));
        server.await.expect("join test WebSocket server");
    })
    .expect("run WebSocket limit test");
}
