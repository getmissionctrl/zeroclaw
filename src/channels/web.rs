//! WebSocket channel for the ZeroClaw web UI.
//!
//! Listens on a dedicated port and bridges browser WebSocket connections
//! into the standard channel dispatch pipeline. Each connected client gets
//! a unique sender identity and full access to the agent loop (tools,
//! streaming, conversation history, cancellation).

use super::traits::{Channel, ChannelMessage, SendMessage};
use async_trait::async_trait;
use axum::{
    extract::{
        ws::{Message, WebSocket},
        State, WebSocketUpgrade,
    },
    response::IntoResponse,
    routing::get,
    Router,
};
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::config::StreamMode;

/// Per-client state stored in the channel.
struct ClientState {
    /// Sender for pushing responses back to this client's WebSocket.
    response_tx: mpsc::Sender<WebResponse>,
}

/// A response message to send back to the client.
#[derive(Debug, Clone, serde::Serialize)]
struct WebResponse {
    #[serde(rename = "type")]
    msg_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(rename = "runId", skip_serializing_if = "Option::is_none")]
    run_id: Option<String>,
}

/// Inbound message from the browser.
#[derive(Debug, serde::Deserialize)]
struct WebRequest {
    #[serde(rename = "type")]
    msg_type: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    images: Vec<String>,
}

/// Shared state for the axum WebSocket server.
#[derive(Clone)]
struct AppState {
    /// Channel sender for dispatching inbound messages to the agent loop.
    channel_tx: mpsc::Sender<ChannelMessage>,
    /// Per-client response senders, keyed by sender identity string.
    clients: Arc<Mutex<HashMap<String, ClientState>>>,
    /// Stream mode configuration.
    stream_mode: StreamMode,
    /// Draft update interval in milliseconds (reserved for future use by the agent loop).
    #[allow(dead_code)]
    draft_update_interval_ms: u64,
}

pub struct WebChannel {
    port: u16,
    bind: String,
    stream_mode: StreamMode,
    draft_update_interval_ms: u64,
    clients: Arc<Mutex<HashMap<String, ClientState>>>,
}

impl WebChannel {
    pub fn new(
        port: u16,
        bind: String,
        stream_mode: StreamMode,
        draft_update_interval_ms: u64,
    ) -> Self {
        Self {
            port,
            bind,
            stream_mode,
            draft_update_interval_ms,
            clients: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Find the response sender for a given recipient (sender identity).
    fn get_response_tx(&self, recipient: &str) -> Option<mpsc::Sender<WebResponse>> {
        self.clients
            .lock()
            .get(recipient)
            .map(|c| c.response_tx.clone())
    }
}

#[async_trait]
impl Channel for WebChannel {
    fn name(&self) -> &str {
        "web"
    }

    async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
        if let Some(tx) = self.get_response_tx(&message.recipient) {
            let resp = WebResponse {
                msg_type: "chat".to_string(),
                state: Some("final".to_string()),
                text: Some(message.content.clone()),
                run_id: None,
            };
            let _ = tx.send(resp).await;
        }
        Ok(())
    }

    async fn listen(&self, tx: mpsc::Sender<ChannelMessage>) -> anyhow::Result<()> {
        let state = AppState {
            channel_tx: tx,
            clients: Arc::clone(&self.clients),
            stream_mode: self.stream_mode,
            draft_update_interval_ms: self.draft_update_interval_ms,
        };

        let app = Router::new()
            .route("/ws", get(ws_upgrade_handler))
            .route("/health", get(|| async { "ok" }))
            .with_state(state);

        let addr: std::net::SocketAddr = format!("{}:{}", self.bind, self.port)
            .parse()
            .map_err(|e| anyhow::anyhow!("Invalid bind address: {e}"))?;

        tracing::info!("WebChannel listening on {addr}");

        let listener = tokio::net::TcpListener::bind(addr).await?;
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .map_err(|e| anyhow::anyhow!("WebChannel server error: {e}"))
    }

    async fn health_check(&self) -> bool {
        true
    }

    async fn start_typing(&self, recipient: &str) -> anyhow::Result<()> {
        if let Some(tx) = self.get_response_tx(recipient) {
            let resp = WebResponse {
                msg_type: "typing".to_string(),
                state: Some("start".to_string()),
                text: None,
                run_id: None,
            };
            let _ = tx.send(resp).await;
        }
        Ok(())
    }

    async fn stop_typing(&self, recipient: &str) -> anyhow::Result<()> {
        if let Some(tx) = self.get_response_tx(recipient) {
            let resp = WebResponse {
                msg_type: "typing".to_string(),
                state: Some("stop".to_string()),
                text: None,
                run_id: None,
            };
            let _ = tx.send(resp).await;
        }
        Ok(())
    }

    fn supports_draft_updates(&self) -> bool {
        self.stream_mode != StreamMode::Off
    }

    async fn send_draft(&self, message: &SendMessage) -> anyhow::Result<Option<String>> {
        if self.stream_mode == StreamMode::Off {
            return Ok(None);
        }

        let draft_id = Uuid::new_v4().to_string();
        if let Some(tx) = self.get_response_tx(&message.recipient) {
            let resp = WebResponse {
                msg_type: "chat".to_string(),
                state: Some("streaming".to_string()),
                text: Some(message.content.clone()),
                run_id: Some(draft_id.clone()),
            };
            let _ = tx.send(resp).await;
        }
        Ok(Some(draft_id))
    }

    async fn update_draft(
        &self,
        recipient: &str,
        message_id: &str,
        text: &str,
    ) -> anyhow::Result<()> {
        if let Some(tx) = self.get_response_tx(recipient) {
            let resp = WebResponse {
                msg_type: "chat".to_string(),
                state: Some("streaming".to_string()),
                text: Some(text.to_string()),
                run_id: Some(message_id.to_string()),
            };
            let _ = tx.send(resp).await;
        }
        Ok(())
    }

    async fn finalize_draft(
        &self,
        recipient: &str,
        message_id: &str,
        text: &str,
    ) -> anyhow::Result<()> {
        if let Some(tx) = self.get_response_tx(recipient) {
            let resp = WebResponse {
                msg_type: "chat".to_string(),
                state: Some("final".to_string()),
                text: Some(text.to_string()),
                run_id: Some(message_id.to_string()),
            };
            let _ = tx.send(resp).await;
        }
        Ok(())
    }

    async fn cancel_draft(&self, recipient: &str, message_id: &str) -> anyhow::Result<()> {
        if let Some(tx) = self.get_response_tx(recipient) {
            let resp = WebResponse {
                msg_type: "chat".to_string(),
                state: Some("cancelled".to_string()),
                text: None,
                run_id: Some(message_id.to_string()),
            };
            let _ = tx.send(resp).await;
        }
        Ok(())
    }
}

async fn ws_upgrade_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws_connection(socket, state))
}

async fn handle_ws_connection(socket: WebSocket, state: AppState) {
    let client_id = format!("web_{}", Uuid::new_v4());
    let (mut ws_tx, mut ws_rx) = socket.split();

    // Create a response channel for this client
    let (resp_tx, mut resp_rx) = mpsc::channel::<WebResponse>(64);

    // Register the client
    state.clients.lock().insert(
        client_id.clone(),
        ClientState {
            response_tx: resp_tx,
        },
    );

    tracing::info!("WebChannel client connected: {client_id}");

    // Task: forward responses from channel dispatch back to the WebSocket
    let client_id_writer = client_id.clone();
    let writer_handle = tokio::spawn(async move {
        while let Some(resp) = resp_rx.recv().await {
            match serde_json::to_string(&resp) {
                Ok(json) => {
                    if ws_tx.send(Message::Text(json.into())).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    tracing::warn!("WebChannel serialize error for {client_id_writer}: {e}");
                }
            }
        }
    });

    // Read loop: client messages → channel dispatch
    while let Some(Ok(msg)) = ws_rx.next().await {
        let text = match msg {
            Message::Text(t) => t.to_string(),
            Message::Close(_) => break,
            _ => continue,
        };

        let req: WebRequest = match serde_json::from_str(&text) {
            Ok(r) => r,
            Err(_) => continue,
        };

        if req.msg_type != "chat" || req.text.trim().is_empty() {
            continue;
        }

        // Build content with image markers if images are present
        let content = if req.images.is_empty() {
            req.text
        } else {
            let markers: Vec<String> = req
                .images
                .iter()
                .map(|url| format!("[IMAGE:{url}]"))
                .collect();
            format!("{}\n{}", req.text, markers.join("\n"))
        };

        let channel_msg = ChannelMessage {
            id: Uuid::new_v4().to_string(),
            sender: client_id.clone(),
            reply_target: client_id.clone(),
            content,
            channel: "web".to_string(),
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            thread_ts: None,
        };

        if state.channel_tx.send(channel_msg).await.is_err() {
            break;
        }
    }

    // Cleanup
    state.clients.lock().remove(&client_id);
    writer_handle.abort();
    tracing::info!("WebChannel client disconnected: {client_id}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_channel_name() {
        let ch = WebChannel::new(5100, "127.0.0.1".into(), StreamMode::Partial, 300);
        assert_eq!(ch.name(), "web");
    }

    #[test]
    fn web_channel_supports_draft_updates_partial() {
        let ch = WebChannel::new(5100, "127.0.0.1".into(), StreamMode::Partial, 300);
        assert!(ch.supports_draft_updates());
    }

    #[test]
    fn web_channel_no_draft_updates_when_off() {
        let ch = WebChannel::new(5100, "127.0.0.1".into(), StreamMode::Off, 300);
        assert!(!ch.supports_draft_updates());
    }

    #[tokio::test]
    async fn send_draft_returns_none_when_off() {
        let ch = WebChannel::new(5100, "127.0.0.1".into(), StreamMode::Off, 300);
        let id = ch
            .send_draft(&SendMessage::new("draft", "web_test"))
            .await
            .unwrap();
        assert!(id.is_none());
    }

    #[tokio::test]
    async fn send_to_missing_client_does_not_error() {
        let ch = WebChannel::new(5100, "127.0.0.1".into(), StreamMode::Partial, 300);
        let result = ch
            .send(&SendMessage::new("hello", "nonexistent_client"))
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn health_check_returns_true() {
        let ch = WebChannel::new(5100, "127.0.0.1".into(), StreamMode::Partial, 300);
        assert!(ch.health_check().await);
    }

    #[test]
    fn web_response_serializes_correctly() {
        let resp = WebResponse {
            msg_type: "chat".to_string(),
            state: Some("streaming".to_string()),
            text: Some("hello".to_string()),
            run_id: Some("run-123".to_string()),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains(r#""type":"chat"#));
        assert!(json.contains(r#""state":"streaming"#));
        assert!(json.contains(r#""runId":"run-123"#));
    }

    #[test]
    fn web_request_deserializes_chat() {
        let json = r#"{"type":"chat","text":"hello","images":[]}"#;
        let req: WebRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.msg_type, "chat");
        assert_eq!(req.text, "hello");
        assert!(req.images.is_empty());
    }

    #[test]
    fn web_request_deserializes_with_images() {
        let json =
            r#"{"type":"chat","text":"look at this","images":["https://example.com/img.png"]}"#;
        let req: WebRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.images.len(), 1);
    }

    #[test]
    fn web_request_defaults_optional_fields() {
        let json = r#"{"type":"chat"}"#;
        let req: WebRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.text, "");
        assert!(req.images.is_empty());
    }

    #[tokio::test]
    async fn ws_round_trip_sends_channel_message() {
        use tokio_tungstenite::connect_async;

        let (channel_tx, mut channel_rx) = mpsc::channel::<ChannelMessage>(16);
        let ch = WebChannel::new(0, "127.0.0.1".into(), StreamMode::Partial, 300);

        // Use port 0 for test (OS picks a free port)
        let state = AppState {
            channel_tx,
            clients: Arc::clone(&ch.clients),
            stream_mode: ch.stream_mode,
            draft_update_interval_ms: ch.draft_update_interval_ms,
        };

        let app = Router::new()
            .route("/ws", get(ws_upgrade_handler))
            .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Start server in background
        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
            .unwrap();
        });

        // Connect WS client
        let url = format!("ws://{addr}/ws");
        let (mut ws, _) = connect_async(&url).await.expect("WS connect failed");

        // Send a chat message
        let msg = serde_json::json!({"type":"chat","text":"hello from test","images":[]});
        ws.send(tokio_tungstenite::tungstenite::Message::Text(
            msg.to_string().into(),
        ))
        .await
        .unwrap();

        // Receive on channel_rx
        let received = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            channel_rx.recv(),
        )
        .await
        .expect("timeout waiting for channel message")
        .expect("channel_rx closed");

        assert_eq!(received.channel, "web");
        assert_eq!(received.content, "hello from test");
        assert!(received.sender.starts_with("web_"));

        // Cleanup
        ws.close(None).await.ok();
    }
}
