use anyhow::Context;
use anyhow::Result;
use clap::Parser;
use futures::SinkExt;
use futures::StreamExt;
use codex_app_server_protocol::ExternalDiffChunkNotification;
use codex_app_server_protocol::ExternalDiffFormat;
use codex_app_server_protocol::ExternalInputQueueKind;
use codex_app_server_protocol::ExternalPermissionRequestKind;
use codex_app_server_protocol::ExternalPermissionRequestNotification;
use codex_app_server_protocol::ExternalPermissionResolvedNotification;
use codex_app_server_protocol::ExternalQueueOperation;
use codex_app_server_protocol::ExternalQueueUpdatedNotification;
use codex_app_server_protocol::ExternalQueuedInput;
use codex_app_server_protocol::ExternalStreamChannel;
use codex_app_server_protocol::ExternalStreamDeltaNotification;
use codex_app_server_protocol::UserInput;
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::path::Path;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::net::UnixListener;
use tokio::net::UnixStream;
use tokio::sync::Mutex;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tracing::warn;
use uuid::Uuid;

/// Unix-socket adapter that transparently forwards Codex app-server JSON-RPC.
///
/// This intentionally reuses built-in app-server methods and notifications as-is:
/// no custom RPC surface is introduced.
#[derive(Parser, Debug, Clone)]
#[command(author, version, about = "Unix-socket passthrough for codex app-server JSON-RPC")]
struct Cli {
    #[arg(long, env = "CODEX_ADAPTER_APP_SERVER_URL")]
    app_server_url: String,
    #[arg(long, env = "CODEX_ADAPTER_SOCKET_PATH")]
    socket_path: String,
}

#[derive(Debug, Clone)]
struct PendingPermission {
    thread_id: String,
    request_id: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if Path::new(&cli.socket_path).exists() {
        let _ = std::fs::remove_file(&cli.socket_path);
    }
    let listener = UnixListener::bind(&cli.socket_path)
        .with_context(|| format!("failed to bind socket {}", cli.socket_path))?;

    loop {
        let (stream, _) = listener.accept().await?;
        let cli = cli.clone();
        tokio::spawn(async move {
            if let Err(err) = handle_connection(stream, &cli.app_server_url).await {
                warn!("adapter connection terminated: {err}");
            }
        });
    }
}

async fn handle_connection(stream: UnixStream, app_server_url: &str) -> Result<()> {
    let (ws_stream, _) = connect_async(app_server_url)
        .await
        .with_context(|| format!("failed to connect to app-server websocket {app_server_url}"))?;
    let (mut ws_write, mut ws_read) = ws_stream.split();

    let (sock_read, sock_write) = stream.into_split();
    let sock_write = std::sync::Arc::new(Mutex::new(sock_write));
    let mut lines = BufReader::new(sock_read).lines();
    let mut pending_permissions: HashMap<String, PendingPermission> = HashMap::new();
    let mut sequence: u64 = 0;

    loop {
        tokio::select! {
            line_res = lines.next_line() => {
                let Some(line) = line_res? else { break };
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if !is_valid_jsonrpc_message(trimmed) {
                    warn!("dropping invalid JSON-RPC input line");
                    continue;
                }

                if let Ok(value) = serde_json::from_str::<JsonValue>(trimmed) {
                    maybe_emit_client_side_notifications(
                        &value,
                        &mut sequence,
                        &sock_write,
                    ).await?;
                    maybe_emit_permission_resolved(
                        &value,
                        &mut pending_permissions,
                        &sock_write,
                    ).await?;
                }

                if let Err(err) = ws_write.send(Message::Text(trimmed.to_string().into())).await {
                    warn!("failed forwarding request to websocket: {err}");
                    break;
                }
            }
            msg = ws_read.next() => {
                let Some(msg) = msg else { break };
                match msg {
                    Ok(Message::Text(text)) => {
                        if !is_valid_jsonrpc_message(&text) {
                            continue;
                        }
                        if let Ok(value) = serde_json::from_str::<JsonValue>(&text) {
                            maybe_capture_permission_request(
                                &value,
                                &mut pending_permissions,
                                &sock_write,
                            ).await?;
                            maybe_emit_stream_notifications(&value, &mut sequence, &sock_write).await?;
                        }
                        write_json_line(&sock_write, &text).await?;
                    }
                    Ok(Message::Binary(_))
                    | Ok(Message::Ping(_))
                    | Ok(Message::Pong(_))
                    | Ok(Message::Frame(_)) => {}
                    Ok(Message::Close(_)) => break,
                    Err(err) => {
                        warn!("websocket read failed: {err}");
                        break;
                    }
                }
            }
        }
    }

    Ok(())
}

async fn maybe_emit_client_side_notifications(
    value: &JsonValue,
    sequence: &mut u64,
    sock_write: &std::sync::Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
) -> Result<()> {
    let Some(obj) = value.as_object() else {
        return Ok(());
    };
    let Some(method) = obj.get("method").and_then(JsonValue::as_str) else {
        return Ok(());
    };
    let params = obj.get("params").cloned().unwrap_or(JsonValue::Null);

    let queue_kind = match method {
        "turn/start" => Some(ExternalInputQueueKind::Interactive),
        "turn/steer" => Some(ExternalInputQueueKind::Steer),
        _ => None,
    };

    if let Some(queue_kind) = queue_kind {
        let thread_id = params
            .get("threadId")
            .and_then(JsonValue::as_str)
            .unwrap_or_default()
            .to_string();
        let expected_turn_id = params
            .get("expectedTurnId")
            .and_then(JsonValue::as_str)
            .map(ToOwned::to_owned);
        let input = parse_input_items(params.get("input"));

        let queue_item = ExternalQueuedInput {
            queue_item_id: format!("q_{}", Uuid::new_v4()),
            thread_id: thread_id.clone(),
            queue_kind,
            input,
            expected_turn_id,
            client_message_id: None,
        };

        let enqueued = ExternalQueueUpdatedNotification {
            thread_id: thread_id.clone(),
            queue_kind,
            operation: ExternalQueueOperation::Enqueued,
            item: Some(queue_item.clone()),
        };
        write_synthetic_notification(
            sock_write,
            "adapter/queueUpdated",
            serde_json::to_value(enqueued)?,
        )
        .await?;

        emit_stream_delta(
            serde_json::json!({
                "threadId": thread_id,
                "turnId": params.get("expectedTurnId").cloned(),
                "delta": serde_json::to_string(&queue_item.input).unwrap_or_default(),
            }),
            ExternalStreamChannel::InputEcho,
            sequence,
            sock_write,
        )
        .await?;

        let dequeued = ExternalQueueUpdatedNotification {
            thread_id: queue_item.thread_id.clone(),
            queue_kind,
            operation: ExternalQueueOperation::Dequeued,
            item: Some(queue_item),
        };
        write_synthetic_notification(
            sock_write,
            "adapter/queueUpdated",
            serde_json::to_value(dequeued)?,
        )
        .await?;
    }

    Ok(())
}

fn parse_input_items(input_value: Option<&JsonValue>) -> Vec<UserInput> {
    let Some(value) = input_value else {
        return Vec::new();
    };
    serde_json::from_value::<Vec<UserInput>>(value.clone()).unwrap_or_default()
}

fn is_valid_jsonrpc_message(raw: &str) -> bool {
    let Ok(value) = serde_json::from_str::<JsonValue>(raw) else {
        return false;
    };
    let Some(obj) = value.as_object() else {
        return false;
    };
    let has_method = obj.get("method").is_some();
    let has_id = obj.get("id").is_some();
    let has_result = obj.get("result").is_some();
    let has_error = obj.get("error").is_some();
    has_method || has_id || has_result || has_error
}

async fn maybe_capture_permission_request(
    value: &JsonValue,
    pending_permissions: &mut HashMap<String, PendingPermission>,
    sock_write: &std::sync::Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
) -> Result<()> {
    let Some(obj) = value.as_object() else {
        return Ok(());
    };
    let Some(method) = obj.get("method").and_then(JsonValue::as_str) else {
        return Ok(());
    };
    let Some(id_json) = obj.get("id") else {
        return Ok(());
    };
    let request_id = jsonrpc_id_to_string(id_json);

    let kind = match method {
        "item/commandExecution/requestApproval" => Some(ExternalPermissionRequestKind::CommandExecution),
        "item/fileChange/requestApproval" => Some(ExternalPermissionRequestKind::FileChange),
        "item/permissions/requestApproval" => Some(ExternalPermissionRequestKind::Permissions),
        "mcpServer/elicitation/request" => Some(ExternalPermissionRequestKind::McpElicitation),
        _ => None,
    };
    let Some(kind) = kind else {
        return Ok(());
    };

    let params = obj.get("params").cloned().unwrap_or(JsonValue::Null);
    let thread_id = params
        .get("threadId")
        .and_then(JsonValue::as_str)
        .unwrap_or_default()
        .to_string();
    let turn_id = params
        .get("turnId")
        .and_then(JsonValue::as_str)
        .map(ToOwned::to_owned);

    pending_permissions.insert(
        request_id.clone(),
        PendingPermission {
            thread_id: thread_id.clone(),
            request_id: request_id.clone(),
        },
    );

    let notification = ExternalPermissionRequestNotification {
        thread_id,
        turn_id,
        request_id,
        kind,
        payload: params,
    };
    write_synthetic_notification(
        sock_write,
        "adapter/permissionRequest",
        serde_json::to_value(notification)?,
    )
    .await
}

async fn maybe_emit_permission_resolved(
    value: &JsonValue,
    pending_permissions: &mut HashMap<String, PendingPermission>,
    sock_write: &std::sync::Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
) -> Result<()> {
    let Some(obj) = value.as_object() else {
        return Ok(());
    };
    let Some(id_json) = obj.get("id") else {
        return Ok(());
    };
    if !(obj.get("result").is_some() || obj.get("error").is_some()) {
        return Ok(());
    }
    let request_id = jsonrpc_id_to_string(id_json);
    let Some(pending) = pending_permissions.remove(&request_id) else {
        return Ok(());
    };

    let approved = if obj.get("error").is_some() {
        false
    } else {
        approval_from_result(obj.get("result"))
    };
    let decision = extract_decision(obj.get("result"));

    let notification = ExternalPermissionResolvedNotification {
        thread_id: pending.thread_id,
        request_id: pending.request_id,
        approved,
        decision,
    };
    write_synthetic_notification(
        sock_write,
        "adapter/permissionResolved",
        serde_json::to_value(notification)?,
    )
    .await
}

async fn maybe_emit_stream_notifications(
    value: &JsonValue,
    sequence: &mut u64,
    sock_write: &std::sync::Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
) -> Result<()> {
    let Some(obj) = value.as_object() else {
        return Ok(());
    };
    let Some(method) = obj.get("method").and_then(JsonValue::as_str) else {
        return Ok(());
    };
    let params = obj.get("params").cloned().unwrap_or(JsonValue::Null);

    match method {
        "item/agentMessage/delta" => {
            emit_stream_delta(
                params,
                ExternalStreamChannel::OutputText,
                sequence,
                sock_write,
            )
            .await?;
        }
        "item/reasoning/summaryTextDelta" => {
            emit_stream_delta(
                params,
                ExternalStreamChannel::ReasoningSummary,
                sequence,
                sock_write,
            )
            .await?;
        }
        "item/reasoning/textDelta" => {
            emit_stream_delta(
                params,
                ExternalStreamChannel::ReasoningContent,
                sequence,
                sock_write,
            )
            .await?;
        }
        "item/commandExecution/outputDelta" => {
            emit_stream_delta(
                params,
                ExternalStreamChannel::TerminalOutput,
                sequence,
                sock_write,
            )
            .await?;
        }
        "item/commandExecution/terminalInteraction" => {
            emit_stream_delta(
                params,
                ExternalStreamChannel::InputEcho,
                sequence,
                sock_write,
            )
            .await?;
        }
        "turn/diff/updated" => {
            let thread_id = params
                .get("threadId")
                .and_then(JsonValue::as_str)
                .unwrap_or_default()
                .to_string();
            let turn_id = params
                .get("turnId")
                .and_then(JsonValue::as_str)
                .unwrap_or_default()
                .to_string();
            let diff = params
                .get("diff")
                .and_then(JsonValue::as_str)
                .unwrap_or_default()
                .to_string();
            let notification = ExternalDiffChunkNotification {
                thread_id,
                turn_id,
                format: ExternalDiffFormat::UnifiedPatch,
                chunk_index: 0,
                complete: true,
                diff,
            };
            write_synthetic_notification(
                sock_write,
                "adapter/diffChunk",
                serde_json::to_value(notification)?,
            )
            .await?;
        }
        "item/completed" => {
            if let Some(item) = params.get("item").and_then(JsonValue::as_object)
                && let Some(item_type) = item.get("type").and_then(JsonValue::as_str)
            {
                match item_type {
                    "agentMessage" => {
                        let text = item
                            .get("text")
                            .and_then(JsonValue::as_str)
                            .unwrap_or_default()
                            .to_string();
                        if !text.is_empty() {
                            emit_stream_delta(
                                serde_json::json!({
                                    "threadId": params.get("threadId").cloned().unwrap_or(JsonValue::Null),
                                    "turnId": params.get("turnId").cloned().unwrap_or(JsonValue::Null),
                                    "delta": text,
                                }),
                                ExternalStreamChannel::OutputText,
                                sequence,
                                sock_write,
                            )
                            .await?;
                        }
                    }
                    "reasoning" => {
                        let summary = item
                            .get("summary")
                            .and_then(JsonValue::as_array)
                            .map(|parts| {
                                parts
                                    .iter()
                                    .filter_map(JsonValue::as_str)
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            })
                            .unwrap_or_default();
                        if !summary.is_empty() {
                            emit_stream_delta(
                                serde_json::json!({
                                    "threadId": params.get("threadId").cloned().unwrap_or(JsonValue::Null),
                                    "turnId": params.get("turnId").cloned().unwrap_or(JsonValue::Null),
                                    "delta": summary,
                                }),
                                ExternalStreamChannel::ReasoningSummary,
                                sequence,
                                sock_write,
                            )
                            .await?;
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    Ok(())
}

async fn emit_stream_delta(
    params: JsonValue,
    channel: ExternalStreamChannel,
    sequence: &mut u64,
    sock_write: &std::sync::Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
) -> Result<()> {
    let thread_id = params
        .get("threadId")
        .and_then(JsonValue::as_str)
        .unwrap_or_default()
        .to_string();
    let turn_id = params
        .get("turnId")
        .and_then(JsonValue::as_str)
        .map(ToOwned::to_owned);
    let delta = params
        .get("delta")
        .or_else(|| params.get("stdin"))
        .and_then(JsonValue::as_str)
        .unwrap_or_default()
        .to_string();
    *sequence = sequence.saturating_add(1);
    let notification = ExternalStreamDeltaNotification {
        thread_id,
        turn_id,
        channel,
        sequence: *sequence,
        delta,
    };
    write_synthetic_notification(
        sock_write,
        "adapter/streamDelta",
        serde_json::to_value(notification)?,
    )
    .await
}

async fn write_json_line(
    sock_write: &std::sync::Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
    line: &str,
) -> Result<()> {
    let mut writer = sock_write.lock().await;
    writer.write_all(line.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

async fn write_synthetic_notification(
    sock_write: &std::sync::Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
    method: &str,
    params: JsonValue,
) -> Result<()> {
    let notification = serde_json::json!({
        "method": method,
        "params": params
    });
    write_json_line(sock_write, &notification.to_string()).await
}

fn jsonrpc_id_to_string(value: &JsonValue) -> String {
    match value {
        JsonValue::String(s) => s.to_string(),
        JsonValue::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

fn approval_from_result(result: Option<&JsonValue>) -> bool {
    let Some(result) = result else {
        return false;
    };
    let Some(obj) = result.as_object() else {
        return false;
    };
    if let Some(decision) = obj.get("decision").and_then(JsonValue::as_str) {
        return decision.starts_with("accept") || decision == "applyNetworkPolicyAmendment";
    }
    if let Some(action) = obj.get("action").and_then(JsonValue::as_str) {
        return action == "accept";
    }
    obj.get("permissions").is_some()
}

fn extract_decision(result: Option<&JsonValue>) -> Option<String> {
    let result = result?;
    let obj = result.as_object()?;
    if let Some(decision) = obj.get("decision").and_then(JsonValue::as_str) {
        return Some(decision.to_string());
    }
    if let Some(action) = obj.get("action").and_then(JsonValue::as_str) {
        return Some(action.to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::is_valid_jsonrpc_message;
    use super::jsonrpc_id_to_string;
    use super::parse_input_items;
    use codex_app_server_protocol::UserInput;

    #[test]
    fn validates_request() {
        assert!(is_valid_jsonrpc_message(
            r#"{"id":"1","method":"thread/start","params":{"model":"gpt-5"}}"#
        ));
    }

    #[test]
    fn validates_notification() {
        assert!(is_valid_jsonrpc_message(
            r#"{"method":"turn/started","params":{"threadId":"thr_1","turnId":"turn_1"}}"#
        ));
    }

    #[test]
    fn rejects_non_jsonrpc() {
        assert!(!is_valid_jsonrpc_message(r#"{"foo":"bar"}"#));
    }

    #[test]
    fn converts_jsonrpc_id_variants() {
        assert_eq!(
            jsonrpc_id_to_string(&serde_json::json!("req-1")),
            "req-1".to_string()
        );
        assert_eq!(
            jsonrpc_id_to_string(&serde_json::json!(42)),
            "42".to_string()
        );
    }

    #[test]
    fn parses_user_input_items_from_turn_start() {
        let parsed = parse_input_items(Some(&serde_json::json!([
            {"type": "text", "text": "hello", "text_elements": []}
        ])));
        assert_eq!(
            parsed,
            vec![UserInput::Text {
                text: "hello".to_string(),
                text_elements: Vec::new(),
            }]
        );
    }
}
