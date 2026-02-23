# WebChannel Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

> **Before starting:** Read `CONTRIBUTING.md` in the repo root for code style, naming conventions, architecture rules, commit conventions, and test requirements. All code must follow those guidelines.

**Goal:** Add a `WebChannel` — a new `Channel` trait implementation that accepts WebSocket connections and bridges them into the existing channel dispatch pipeline, giving the web UI full access to the agent loop (tools, streaming, conversation history, cancellation).

**Architecture:** The `WebChannel` listens for WebSocket connections on a configurable port (default 5100, separate from the gateway's port 3000). Each connected client gets its own sender identity and conversation history, just like Telegram or Discord users. The channel supports draft updates for streaming, so partial responses are pushed to the browser as they're generated. The web UI's Node.js server proxies browser WebSocket connections to this channel.

**Tech Stack:** Rust, axum (already a dependency with `ws` feature enabled), tokio, tokio-tungstenite (already a dependency). No new crates needed.

**Protocol:** The WebChannel uses JSON text frames:
- Client → Channel: `{"type":"chat","text":"...","images":[]}`
- Channel → Client (streaming): `{"type":"chat","state":"streaming","runId":"...","text":"..."}`
- Channel → Client (final): `{"type":"chat","state":"final","runId":"...","text":"..."}`
- Client → Channel (widget): `{"type":"widget_response","id":"...","widget":"...","value":"..."}`

---

## Pre-Flight: Sync with Upstream

### Task 0: Pull latest and create feature branch

**Step 1: Pull latest from upstream**

Run: `cd ~/dev/agents/zeroclaw && git fetch upstream && git merge upstream/main`
Expected: Fast-forward merge or clean merge.

**Step 2: Verify build and tests pass on latest**

Run: `cargo build && cargo test --locked`
Expected: Compiles and all tests pass. If tests fail, investigate before proceeding.

**Step 3: Enable git hooks**

Run: `git config core.hooksPath .githooks`

**Step 4: Create feature branch**

Run: `git checkout -b feat/web-channel`

---

## Phase 1: Config Schema

### Task 1: Add `WebConfig` to `ChannelsConfig`

**Files:**
- Modify: `src/config/schema.rs`

**Step 1: Add `WebConfig` struct**

Add after the `QQConfig` struct (search for `pub struct QQConfig` to find the right spot):

```rust
/// Web UI WebSocket channel configuration.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WebConfig {
    /// Port to listen on for WebSocket connections.
    #[serde(default = "default_web_port")]
    pub port: u16,
    /// Bind address for the WebSocket server.
    #[serde(default = "default_web_bind")]
    pub bind: String,
    /// Streaming mode (off or partial). Default: partial.
    #[serde(default = "default_web_stream_mode")]
    pub stream_mode: StreamMode,
    /// Draft update flush interval in milliseconds.
    #[serde(default = "default_web_draft_update_interval_ms")]
    pub draft_update_interval_ms: u64,
    /// Optional list of allowed origin hosts for CORS. Empty = allow all.
    #[serde(default)]
    pub allowed_origins: Vec<String>,
}

fn default_web_port() -> u16 {
    5100
}

fn default_web_bind() -> String {
    "127.0.0.1".to_string()
}

fn default_web_stream_mode() -> StreamMode {
    StreamMode::Partial
}

fn default_web_draft_update_interval_ms() -> u64 {
    300
}
```

**Step 2: Add `web` field to `ChannelsConfig`**

In the `ChannelsConfig` struct (line ~2167), add after the `qq` field:

```rust
    /// Web UI WebSocket channel configuration.
    pub web: Option<WebConfig>,
```

**Step 3: Add `web: None` to `ChannelsConfig::default()`**

In the `impl Default for ChannelsConfig` block (line ~2213), add after `qq: None,`:

```rust
            web: None,
```

**Step 4: Run tests to verify schema compiles**

Run: `cargo test --locked -p zeroclaw --lib config`
Expected: PASS (schema tests pass, no breakage)

**Step 5: Commit**

```bash
git add src/config/schema.rs
git commit -m "feat(channel): add WebConfig schema for web UI WebSocket channel"
```

---

## Phase 2: WebChannel Implementation

### Task 2: Create the WebChannel struct and Channel trait implementation

**Files:**
- Create: `src/channels/web.rs`
- Modify: `src/channels/mod.rs` (add module declaration and re-export)

This is the core implementation. The `WebChannel`:
1. Runs an axum HTTP+WS server on its own port
2. Upgrades `/ws` connections to WebSocket
3. Reads JSON messages from clients and sends them as `ChannelMessage` via the mpsc sender
4. Receives responses via an internal per-client response channel and sends them back as JSON frames
5. Supports draft updates for streaming

**Step 1: Create `src/channels/web.rs` with the struct, config, and axum server**

```rust
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
    /// Draft update interval in milliseconds.
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
        self.clients.lock().get(recipient).map(|c| c.response_tx.clone())
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
        use futures_util::SinkExt;
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
    use futures_util::StreamExt;
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
        let json = r#"{"type":"chat","text":"look at this","images":["https://example.com/img.png"]}"#;
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
}
```

Note: This file uses `futures_util::{SinkExt, StreamExt}` for the WebSocket split pattern. Axum re-exports these via its WebSocket types, but if the compiler complains, `futures-util` may need adding to `Cargo.toml` dependencies. Check if it's already present:

Run: `grep futures-util Cargo.toml`

If missing, add: `futures-util = "0.3"` to `[dependencies]`.

**Step 2: Register the module in `src/channels/mod.rs`**

Add after line 31 (`pub mod telegram;`):

```rust
pub mod web;
```

Add after line 53 (`pub use telegram::TelegramChannel;`):

```rust
pub use web::WebChannel;
```

**Step 3: Run tests**

Run: `cargo test --locked -p zeroclaw --lib channels::web`
Expected: All web channel tests pass.

**Step 4: Commit**

```bash
git add src/channels/web.rs src/channels/mod.rs
git commit -m "feat(channel): add WebChannel with WebSocket support for web UI"
```

---

## Phase 3: Wire WebChannel into Channel Startup

### Task 3: Register WebChannel in `start_channels()`

**Files:**
- Modify: `src/channels/mod.rs` (in the `start_channels()` function, line ~2211)

**Step 1: Add WebChannel construction**

In `start_channels()`, find the block where channels are constructed (after ~line 2492, after the `linq` channel block). Add before the `email` channel block:

```rust
    if let Some(ref web_cfg) = config.channels_config.web {
        channels.push(Arc::new(WebChannel::new(
            web_cfg.port,
            web_cfg.bind.clone(),
            web_cfg.stream_mode,
            web_cfg.draft_update_interval_ms,
        )));
    }
```

**Step 2: Run full test suite**

Run: `cargo test --locked`
Expected: All tests pass — no existing behavior changed.

**Step 3: Commit**

```bash
git add src/channels/mod.rs
git commit -m "feat(channel): wire WebChannel into start_channels() dispatch"
```

---

## Phase 4: Integration Test

### Task 4: WebSocket round-trip integration test

**Files:**
- Modify: `src/channels/web.rs` (add integration test at bottom of `#[cfg(test)]` block)

**Step 1: Add an integration test that starts the server and connects a WS client**

Add to the `#[cfg(test)] mod tests` block in `src/channels/web.rs`:

```rust
    #[tokio::test]
    async fn ws_round_trip_sends_channel_message() {
        use tokio_tungstenite::connect_async;
        use futures_util::{SinkExt, StreamExt};

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
        ws.send(tokio_tungstenite::tungstenite::Message::Text(msg.to_string()))
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
```

**Step 2: Run integration test**

Run: `cargo test --locked -p zeroclaw --lib channels::web::tests::ws_round_trip`
Expected: PASS

**Step 3: Commit**

```bash
git add src/channels/web.rs
git commit -m "test(channel): add WebSocket round-trip integration test for WebChannel"
```

---

## Phase 5: Documentation

### Task 5: Update README and add config example

**Files:**
- Modify: `README.md` (add web channel to supported channels list, if one exists)

**Step 1: Add config example to README or config docs**

Find the channels documentation section and add:

```toml
# Web UI WebSocket channel (for browser-based chat)
[channels_config.web]
port = 5100                      # WebSocket server port
bind = "127.0.0.1"               # Bind address
stream_mode = "partial"          # Enable streaming (partial updates)
draft_update_interval_ms = 300   # Flush interval for streaming chunks
```

**Step 2: Run quality gate**

Run: `./scripts/ci/rust_quality_gate.sh`
Expected: PASS

**Step 3: Run full test suite one final time**

Run: `cargo test --locked`
Expected: All tests pass.

**Step 4: Commit**

```bash
git add -A
git commit -m "docs: add WebChannel configuration example"
```

---

## Summary of Changes

| File | Change |
|------|--------|
| `src/config/schema.rs` | Add `WebConfig` struct, add `web: Option<WebConfig>` to `ChannelsConfig` |
| `src/channels/web.rs` | **New file** — `WebChannel` implementation with axum WS server |
| `src/channels/mod.rs` | Add `pub mod web; pub use web::WebChannel;` and registration in `start_channels()` |
| `README.md` | Add web channel config example |

**Total new code:** ~350 lines (implementation) + ~120 lines (tests)
**New dependencies:** None (axum `ws` + tokio-tungstenite already in Cargo.toml). Possibly `futures-util` if not already present.

---

## Post-Merge: Web UI Integration

After this PR lands, the zeroclaw web UI (`scape-agents/packages/zeroclaw-ui`) needs its gateway proxy updated:

1. Change `GATEWAY_URL` default from `http://127.0.0.1:3000` to `ws://127.0.0.1:5100`
2. Revert gateway-proxy.ts from HTTP-to-WS bridging back to direct WS-to-WS forwarding
3. The protocol matches: `{"type":"chat","state":"streaming"|"final","runId":"...","text":"..."}`

This is tracked separately in the `scape-agents` repo.
