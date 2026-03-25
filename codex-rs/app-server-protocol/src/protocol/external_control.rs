use std::path::PathBuf;

use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::Personality;
use codex_protocol::openai_models::ReasoningEffort;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use ts_rs::TS;

use crate::protocol::v2::UserInput;

/// Preferred input queue for externally submitted messages.
///
/// - `Interactive`: normal user submissions.
/// - `FollowUp`: queued follow-up requests from external controllers.
/// - `Steer`: control/steering messages injected during an active turn.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum ExternalInputQueueKind {
    Interactive,
    FollowUp,
    Steer,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum ExternalQueueOperation {
    Enqueued,
    Dequeued,
    Dropped,
    Cleared,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ExternalQueuedInput {
    pub queue_item_id: String,
    pub thread_id: String,
    pub queue_kind: ExternalInputQueueKind,
    pub input: Vec<UserInput>,
    #[ts(optional = nullable)]
    pub expected_turn_id: Option<String>,
    #[ts(optional = nullable)]
    pub client_message_id: Option<String>,
}

/// Enqueue input for a specific external queue.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ExternalQueueInputEnqueueParams {
    pub thread_id: String,
    pub queue_kind: ExternalInputQueueKind,
    pub input: Vec<UserInput>,
    #[ts(optional = nullable)]
    pub expected_turn_id: Option<String>,
    #[ts(optional = nullable)]
    pub client_message_id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ExternalQueueInputEnqueueResponse {
    pub queue_item_id: String,
}

/// List queued input items for one queue, or all queues when `queue_kind` is null.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ExternalQueueInputListParams {
    pub thread_id: String,
    #[ts(optional = nullable)]
    pub queue_kind: Option<ExternalInputQueueKind>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ExternalQueueInputListResponse {
    pub items: Vec<ExternalQueuedInput>,
}

/// Remove a single queued item by id.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ExternalQueueInputDropParams {
    pub thread_id: String,
    pub queue_item_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ExternalQueueInputDropResponse {}

/// Clear one queue, or all queues when `queue_kind` is null.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ExternalQueueInputClearParams {
    pub thread_id: String,
    #[ts(optional = nullable)]
    pub queue_kind: Option<ExternalInputQueueKind>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ExternalQueueInputClearResponse {}

/// Runtime control surface that must route through the same thread mutations as UI operations.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ExternalRuntimeControlSetParams {
    pub thread_id: String,
    #[ts(optional = nullable)]
    pub model: Option<String>,
    #[ts(optional = nullable)]
    pub personality: Option<Personality>,
    #[ts(optional = nullable)]
    pub reasoning_effort: Option<ReasoningEffort>,
    #[ts(optional = nullable)]
    pub collaboration_mode: Option<CollaborationMode>,
    #[ts(optional = nullable)]
    pub cwd: Option<PathBuf>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ExternalRuntimeControlSetResponse {}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ExternalQueueUpdatedNotification {
    pub thread_id: String,
    pub queue_kind: ExternalInputQueueKind,
    pub operation: ExternalQueueOperation,
    #[ts(optional = nullable)]
    pub item: Option<ExternalQueuedInput>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum ExternalStreamChannel {
    InputEcho,
    OutputText,
    ReasoningSummary,
    ReasoningContent,
    ToolLifecycle,
    TerminalOutput,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ExternalStreamDeltaNotification {
    pub thread_id: String,
    #[ts(optional = nullable)]
    pub turn_id: Option<String>,
    pub channel: ExternalStreamChannel,
    /// Monotonic sequence for external consumers to detect missed events.
    pub sequence: u64,
    pub delta: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum ExternalPermissionRequestKind {
    CommandExecution,
    FileChange,
    Permissions,
    McpElicitation,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ExternalPermissionRequestNotification {
    pub thread_id: String,
    #[ts(optional = nullable)]
    pub turn_id: Option<String>,
    pub request_id: String,
    pub kind: ExternalPermissionRequestKind,
    /// Raw payload mirrors the app-server request params shape.
    pub payload: JsonValue,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ExternalPermissionResolvedNotification {
    pub thread_id: String,
    pub request_id: String,
    pub approved: bool,
    #[ts(optional = nullable)]
    pub decision: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum ExternalDiffFormat {
    UnifiedPatch,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ExternalDiffChunkNotification {
    pub thread_id: String,
    pub turn_id: String,
    pub format: ExternalDiffFormat,
    pub chunk_index: u64,
    pub complete: bool,
    pub diff: String,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn queue_enqueue_params_round_trip() {
        let value = json!({
            "threadId": "thr_123",
            "queueKind": "followUp",
            "input": [
                { "type": "text", "text": "check test status", "text_elements": [] }
            ],
            "expectedTurnId": "turn_9",
            "clientMessageId": "msg_1"
        });

        let params = serde_json::from_value::<ExternalQueueInputEnqueueParams>(value.clone())
            .expect("deserialize params");
        assert_eq!(params.queue_kind, ExternalInputQueueKind::FollowUp);
        assert_eq!(
            serde_json::to_value(params).expect("serialize params"),
            value
        );
    }

    #[test]
    fn stream_delta_round_trip() {
        let value = json!({
            "threadId": "thr_123",
            "turnId": "turn_1",
            "channel": "reasoningContent",
            "sequence": 42,
            "delta": "Thinking..."
        });
        let msg =
            serde_json::from_value::<ExternalStreamDeltaNotification>(value.clone()).unwrap();
        assert_eq!(msg.channel, ExternalStreamChannel::ReasoningContent);
        assert_eq!(serde_json::to_value(msg).unwrap(), value);
    }
}
