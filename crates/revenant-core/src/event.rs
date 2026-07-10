//! Runtime event bus payloads. One broadcast channel feeds the control-plane
//! SSE stream, the TUI, and channel adapters; everything is serializable.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    TurnStarted {
        session_id: i64,
    },
    /// Streaming text delta for a session's in-flight turn.
    TurnDelta {
        session_id: i64,
        text: String,
    },
    ToolStarted {
        session_id: i64,
        tool: String,
        summary: String,
    },
    ToolFinished {
        session_id: i64,
        tool: String,
        ok: bool,
    },
    TurnCompleted {
        session_id: i64,
        text: String,
        input_tokens: u64,
        output_tokens: u64,
        routed_model: Option<String>,
    },
    TurnFailed {
        session_id: i64,
        error: String,
    },
    ApprovalCreated {
        id: String,
        session_id: i64,
        kind: String,
        summary: String,
        expires_at: i64,
    },
    ApprovalResolved {
        id: String,
        verdict: String,
        resolver: String,
    },
    SubagentSpawned {
        parent_session: i64,
        child_session: i64,
        task: String,
        tier: String,
    },
    SubagentFinished {
        parent_session: i64,
        child_session: i64,
        ok: bool,
    },
    LoopCompleted {
        loop_id: String,
        name: String,
        channel_out: String,
        text: String,
    },
    GatewayStatus {
        healthy: bool,
        detail: String,
    },
}

impl Event {
    pub fn session_id(&self) -> Option<i64> {
        match self {
            Event::TurnStarted { session_id }
            | Event::TurnDelta { session_id, .. }
            | Event::ToolStarted { session_id, .. }
            | Event::ToolFinished { session_id, .. }
            | Event::TurnCompleted { session_id, .. }
            | Event::TurnFailed { session_id, .. }
            | Event::ApprovalCreated { session_id, .. } => Some(*session_id),
            Event::SubagentSpawned { parent_session, .. }
            | Event::SubagentFinished { parent_session, .. } => Some(*parent_session),
            _ => None,
        }
    }
}

/// Cheap clonable handle over a broadcast channel.
#[derive(Clone)]
pub struct EventBus {
    tx: tokio::sync::broadcast::Sender<Event>,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = tokio::sync::broadcast::channel(capacity);
        EventBus { tx }
    }
    pub fn emit(&self, event: Event) {
        let _ = self.tx.send(event); // no receivers is fine
    }
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Event> {
        self.tx.subscribe()
    }
}
