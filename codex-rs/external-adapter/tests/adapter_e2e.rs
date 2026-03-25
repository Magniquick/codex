use anyhow::Context;
use anyhow::Result;
use app_test_support::create_apply_patch_sse_response;
use app_test_support::create_final_assistant_message_sse_response;
use app_test_support::create_mock_responses_server_sequence;
use codex_app_server_protocol::ExternalDiffChunkNotification;
use codex_app_server_protocol::ExternalPermissionRequestNotification;
use codex_app_server_protocol::ExternalPermissionResolvedNotification;
use codex_app_server_protocol::ExternalQueueUpdatedNotification;
use codex_app_server_protocol::ExternalStreamChannel;
use codex_app_server_protocol::ExternalStreamDeltaNotification;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::JSONRPCNotification;
use codex_app_server_protocol::JSONRPCRequest;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::FileChangeApprovalDecision;
use codex_app_server_protocol::FileChangeRequestApprovalResponse;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::path::Path;
use std::process::Stdio;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};
use tokio::time::{timeout, Duration, Instant, sleep};

const READ_TIMEOUT: Duration = Duration::from_secs(10);

#[tokio::test]
async fn adapter_emits_stream_diff_permission_and_queue_notifications() -> Result<()> {
    let tmp = TempDir::new()?;
    let codex_home = tmp.path().join("codex_home");
    std::fs::create_dir(&codex_home)?;
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir(&workspace)?;

    let patch = r#"*** Begin Patch
*** Add File: README.md
+new line
*** End Patch
"#;
    let responses = vec![
        create_apply_patch_sse_response(patch, "patch-call")?,
        create_final_assistant_message_sse_response("patched")?,
    ];
    let model_server = create_mock_responses_server_sequence(responses).await;
    write_mock_config(&codex_home, &model_server.uri())?;

    let (mut app_server, bind_addr) = spawn_websocket_server(&codex_home).await?;

    let socket_path = tmp.path().join("codex-adapter.sock");
    let mut adapter = spawn_adapter(&socket_path, &format!("ws://{bind_addr}")).await?;
    wait_for_socket(&socket_path).await?;

    let stream = UnixStream::connect(&socket_path).await?;
    let (sock_read, mut sock_write) = stream.into_split();
    let mut lines = BufReader::new(sock_read).lines();

    send_jsonrpc_request(
        &mut sock_write,
        "initialize",
        1,
        Some(json!({
            "clientInfo": {
                "name": "external-adapter-e2e",
                "title": "External Adapter E2E",
                "version": "0.1.0"
            },
            "capabilities": {
                "experimentalApi": true
            }
        })),
    )
    .await?;
    wait_for_response_id(&mut lines, 1).await?;

    send_jsonrpc_notification(&mut sock_write, "notifications/initialized", None).await?;

    send_jsonrpc_request(
        &mut sock_write,
        "thread/start",
        2,
        Some(json!({
            "model": "mock-model",
            "cwd": workspace.to_string_lossy().to_string()
        })),
    )
    .await?;
    let thread_start: ThreadStartResponse =
        extract_response_result(wait_for_response_id(&mut lines, 2).await?)?;

    send_jsonrpc_request(
        &mut sock_write,
        "turn/start",
        3,
        Some(json!({
            "threadId": thread_start.thread.id,
            "cwd": workspace,
            "input": [
                {"type": "text", "text": "apply patch", "text_elements": []}
            ]
        })),
    )
    .await?;
    let mut saw_queue_enqueued = false;
    let mut saw_queue_dequeued = false;
    let mut saw_stream_delta = false;
    let mut saw_input_echo = false;
    let turn_start: TurnStartResponse = loop {
        let message = read_next_jsonrpc(&mut lines).await?;
        match message {
            JSONRPCMessage::Response(response) if response.id == RequestId::Integer(3) => {
                break extract_response_result(response)?;
            }
            JSONRPCMessage::Notification(notification) if notification.method == "adapter/queueUpdated" => {
                let payload: ExternalQueueUpdatedNotification =
                    serde_json::from_value(notification.params.context("queue params")?)?;
                if payload.operation == codex_app_server_protocol::ExternalQueueOperation::Enqueued {
                    saw_queue_enqueued = true;
                }
                if payload.operation == codex_app_server_protocol::ExternalQueueOperation::Dequeued {
                    saw_queue_dequeued = true;
                }
            }
            JSONRPCMessage::Notification(notification) if notification.method == "adapter/streamDelta" => {
                let payload: ExternalStreamDeltaNotification =
                    serde_json::from_value(notification.params.context("stream params")?)?;
                if !payload.delta.is_empty() {
                    saw_stream_delta = true;
                }
                if payload.channel == ExternalStreamChannel::InputEcho {
                    saw_input_echo = true;
                }
            }
            _ => {}
        }
    };
    let mut saw_diff_chunk = false;
    let mut permission_request_id: Option<String> = None;

    let deadline = Instant::now() + READ_TIMEOUT;
    while Instant::now() < deadline {
        let message = read_next_jsonrpc(&mut lines).await?;
        let JSONRPCMessage::Notification(notification) = message else {
            continue;
        };

        match notification.method.as_str() {
            "adapter/queueUpdated" => {
                let payload: ExternalQueueUpdatedNotification =
                    serde_json::from_value(notification.params.context("queue params")?)?;
                if payload.operation == codex_app_server_protocol::ExternalQueueOperation::Enqueued {
                    saw_queue_enqueued = true;
                }
                if payload.operation == codex_app_server_protocol::ExternalQueueOperation::Dequeued {
                    saw_queue_dequeued = true;
                }
            }
            "adapter/streamDelta" => {
                let payload: ExternalStreamDeltaNotification =
                    serde_json::from_value(notification.params.context("stream params")?)?;
                if !payload.delta.is_empty() {
                    saw_stream_delta = true;
                }
                if payload.channel == ExternalStreamChannel::InputEcho {
                    saw_input_echo = true;
                }
            }
            "adapter/diffChunk" => {
                let payload: ExternalDiffChunkNotification =
                    serde_json::from_value(notification.params.context("diff params")?)?;
                if payload.turn_id == turn_start.turn.id && payload.diff.contains("README.md") {
                    saw_diff_chunk = true;
                }
            }
            "adapter/permissionRequest" => {
                let payload: ExternalPermissionRequestNotification =
                    serde_json::from_value(notification.params.context("permission request params")?)?;
                permission_request_id = Some(payload.request_id);
                break;
            }
            _ => {}
        }
    }

    let request_id = permission_request_id.context("did not observe permission request")?;
    assert!(saw_queue_enqueued, "expected adapter queue enqueue notification");
    assert!(saw_queue_dequeued, "expected adapter queue dequeue notification");

    send_jsonrpc_result(
        &mut sock_write,
        request_id.clone(),
        json!(FileChangeRequestApprovalResponse {
            decision: FileChangeApprovalDecision::Accept,
        }),
    )
    .await?;

    let mut saw_permission_resolved = false;
    let deadline = Instant::now() + READ_TIMEOUT;
    while Instant::now() < deadline {
        let message = read_next_jsonrpc(&mut lines).await?;
        let JSONRPCMessage::Notification(notification) = message else {
            continue;
        };
        match notification.method.as_str() {
            "adapter/permissionResolved" => {
                let payload: ExternalPermissionResolvedNotification =
                    serde_json::from_value(notification.params.context("permission resolved params")?)?;
                if payload.request_id == request_id {
                    saw_permission_resolved = true;
                }
            }
            "adapter/streamDelta" => {
                let payload: ExternalStreamDeltaNotification =
                    serde_json::from_value(notification.params.context("stream params")?)?;
                if !payload.delta.is_empty() {
                    saw_stream_delta = true;
                }
                if payload.channel == ExternalStreamChannel::InputEcho {
                    saw_input_echo = true;
                }
            }
            "adapter/diffChunk" => {
                let payload: ExternalDiffChunkNotification =
                    serde_json::from_value(notification.params.context("diff params")?)?;
                if payload.turn_id == turn_start.turn.id && payload.diff.contains("README.md") {
                    saw_diff_chunk = true;
                }
            }
            "turn/completed" => break,
            _ => {}
        }
    }

    assert!(saw_input_echo, "expected input-echo stream delta from adapter");
    assert!(saw_stream_delta, "expected at least one stream delta from adapter");
    assert!(saw_diff_chunk, "expected parseable diff chunk notification");
    assert!(saw_permission_resolved, "expected permission resolved notification");

    let readme_path = workspace.join("README.md");
    assert_eq!(std::fs::read_to_string(readme_path)?, "new line\n");

    stop_child(&mut adapter, "adapter").await?;
    stop_child(&mut app_server, "app-server").await?;

    Ok(())
}

fn write_mock_config(codex_home: &Path, server_uri: &str) -> Result<()> {
    std::fs::write(
        codex_home.join("config.toml"),
        format!(
            r#"
model = "mock-model"
approval_policy = "untrusted"
sandbox_mode = "workspace-write"
model_provider = "mock_provider"

[model_providers.mock_provider]
name = "Mock provider"
base_url = "{server_uri}/v1"
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0
supports_websockets = false
"#
        ),
    )?;
    Ok(())
}

async fn spawn_websocket_server(codex_home: &Path) -> Result<(Child, std::net::SocketAddr)> {
    let program = codex_utils_cargo_bin::cargo_bin("codex-app-server")
        .context("should find app-server binary")?;
    let mut cmd = Command::new(program);
    cmd.arg("--listen")
        .arg("ws://127.0.0.1:0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .env("CODEX_HOME", codex_home)
        .env("RUST_LOG", "debug");

    let mut process = cmd
        .kill_on_drop(true)
        .spawn()
        .context("failed to spawn websocket app-server")?;

    let stderr = process
        .stderr
        .take()
        .context("failed to capture app-server stderr")?;
    let mut stderr_reader = BufReader::new(stderr).lines();

    let deadline = Instant::now() + READ_TIMEOUT;
    let bind_addr = loop {
        let line = timeout(
            deadline.saturating_duration_since(Instant::now()),
            stderr_reader.next_line(),
        )
        .await
        .context("timed out waiting for app-server bind output")?
        .context("failed reading app-server stderr")?
        .context("app-server exited before reporting websocket address")?;

        if let Some(addr) = line
            .split_whitespace()
            .find_map(|token| token.strip_prefix("ws://"))
            .and_then(|s| s.parse::<std::net::SocketAddr>().ok())
        {
            break addr;
        }
    };

    tokio::spawn(async move {
        while let Ok(Some(line)) = stderr_reader.next_line().await {
            eprintln!("[app-server stderr] {line}");
        }
    });

    Ok((process, bind_addr))
}

async fn spawn_adapter(socket_path: &Path, app_server_url: &str) -> Result<Child> {
    let program = codex_utils_cargo_bin::cargo_bin("codex-external-adapter")
        .context("should find external-adapter binary")?;
    let mut cmd = Command::new(program);
    cmd.arg("--app-server-url")
        .arg(app_server_url)
        .arg("--socket-path")
        .arg(socket_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd.spawn().context("failed to spawn external-adapter")?;
    if let Some(stderr) = child.stderr.take() {
        let mut reader = BufReader::new(stderr).lines();
        tokio::spawn(async move {
            while let Ok(Some(line)) = reader.next_line().await {
                eprintln!("[adapter stderr] {line}");
            }
        });
    }
    Ok(child)
}

async fn wait_for_socket(socket_path: &Path) -> Result<()> {
    let deadline = Instant::now() + READ_TIMEOUT;
    while Instant::now() < deadline {
        if socket_path.exists() {
            return Ok(());
        }
        sleep(Duration::from_millis(50)).await;
    }
    anyhow::bail!("adapter socket was not created in time")
}

async fn send_jsonrpc_request(
    sock_write: &mut tokio::net::unix::OwnedWriteHalf,
    method: &str,
    id: i64,
    params: Option<serde_json::Value>,
) -> Result<()> {
    let request = JSONRPCMessage::Request(JSONRPCRequest {
        id: RequestId::Integer(id),
        method: method.to_string(),
        params,
        trace: None,
    });
    write_json_line(sock_write, &request).await
}

async fn send_jsonrpc_notification(
    sock_write: &mut tokio::net::unix::OwnedWriteHalf,
    method: &str,
    params: Option<serde_json::Value>,
) -> Result<()> {
    let notification = JSONRPCMessage::Notification(JSONRPCNotification {
        method: method.to_string(),
        params,
    });
    write_json_line(sock_write, &notification).await
}

async fn send_jsonrpc_result(
    sock_write: &mut tokio::net::unix::OwnedWriteHalf,
    request_id: String,
    result: serde_json::Value,
) -> Result<()> {
    let response = JSONRPCMessage::Response(JSONRPCResponse {
        id: if let Ok(number) = request_id.parse::<i64>() {
            RequestId::Integer(number)
        } else {
            RequestId::String(request_id)
        },
        result,
    });
    write_json_line(sock_write, &response).await
}

async fn write_json_line(
    sock_write: &mut tokio::net::unix::OwnedWriteHalf,
    message: &JSONRPCMessage,
) -> Result<()> {
    let payload = serde_json::to_string(message)?;
    sock_write.write_all(payload.as_bytes()).await?;
    sock_write.write_all(b"\n").await?;
    sock_write.flush().await?;
    Ok(())
}

async fn read_next_jsonrpc(
    lines: &mut tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
) -> Result<JSONRPCMessage> {
    let line = timeout(READ_TIMEOUT, lines.next_line())
        .await
        .context("timed out waiting for adapter message")??
        .context("adapter stream closed unexpectedly")?;
    Ok(serde_json::from_str::<JSONRPCMessage>(&line)
        .with_context(|| format!("invalid JSON-RPC line from adapter: {line}"))?)
}

async fn wait_for_response_id(
    lines: &mut tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    id: i64,
) -> Result<JSONRPCResponse> {
    let target = RequestId::Integer(id);
    let deadline = Instant::now() + READ_TIMEOUT;
    while Instant::now() < deadline {
        let message = read_next_jsonrpc(lines).await?;
        if let JSONRPCMessage::Response(response) = message
            && response.id == target
        {
            return Ok(response);
        }
    }
    anyhow::bail!("did not receive response for request id {id}")
}

fn extract_response_result<T: serde::de::DeserializeOwned>(response: JSONRPCResponse) -> Result<T> {
    Ok(serde_json::from_value(response.result)?)
}

async fn stop_child(child: &mut Child, name: &str) -> Result<()> {
    child
        .kill()
        .await
        .with_context(|| format!("failed to kill {name}"))?;
    Ok(())
}
