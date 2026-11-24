//! SSE push events (contracts §1.9).
//!
//! Frame: `id: <seq>\nevent: <type>\ndata: <json>\n\n`.

use crate::api::{AttentionItem, DeliveryReport, DispatchTarget};
use crate::event::NormalizedEvent;
use crate::id::{ArtifactId, PipelineId, ReviewId};
use crate::pipeline::{ArtifactKind, PipelineSpec};
use crate::session::{HostRecord, SessionRecord};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum DaemonEvent {
    Event {
        seq: u64,
        event: NormalizedEvent,
    },
    Session {
        seq: u64,
        session: SessionRecord,
    },
    Pipeline {
        seq: u64,
        pipeline: PipelineSpec,
    },
    Artifact {
        seq: u64,
        id: ArtifactId,
        kind: ArtifactKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pipeline_id: Option<PipelineId>,
    },
    Review {
        seq: u64,
        review_id: ReviewId,
    },
    Delivery {
        seq: u64,
        report: DeliveryReport,
        target: DispatchTarget,
    },
    Attention {
        seq: u64,
        items: Vec<AttentionItem>,
    },
    Host {
        seq: u64,
        host: HostRecord,
    },
}

impl DaemonEvent {
    pub fn seq(&self) -> u64 {
        match self {
            DaemonEvent::Event { seq, .. }
            | DaemonEvent::Session { seq, .. }
            | DaemonEvent::Pipeline { seq, .. }
            | DaemonEvent::Artifact { seq, .. }
            | DaemonEvent::Review { seq, .. }
            | DaemonEvent::Delivery { seq, .. }
            | DaemonEvent::Attention { seq, .. }
            | DaemonEvent::Host { seq, .. } => *seq,
        }
    }

    /// The SSE `event:` name (same as the `type` tag).
    pub fn event_name(&self) -> &'static str {
        match self {
            DaemonEvent::Event { .. } => "event",
            DaemonEvent::Session { .. } => "session",
            DaemonEvent::Pipeline { .. } => "pipeline",
            DaemonEvent::Artifact { .. } => "artifact",
            DaemonEvent::Review { .. } => "review",
            DaemonEvent::Delivery { .. } => "delivery",
            DaemonEvent::Attention { .. } => "attention",
            DaemonEvent::Host { .. } => "host",
        }
    }

    /// Render a complete SSE frame (JSON never contains raw newlines).
    pub fn to_sse_frame(&self) -> Result<String, serde_json::Error> {
        let data = serde_json::to_string(self)?;
        Ok(format!(
            "id: {}\nevent: {}\ndata: {}\n\n",
            self.seq(),
            self.event_name(),
            data
        ))
    }
}

/// Ring-buffer capacity for `Last-Event-ID` replay.
pub const SSE_REPLAY_CAPACITY: usize = 1000;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_format() {
        let e = DaemonEvent::Review {
            seq: 42,
            review_id: ReviewId::new("R-1"),
        };
        let f = e.to_sse_frame().unwrap();
        assert_eq!(
            f,
            "id: 42\nevent: review\ndata: {\"type\":\"review\",\"seq\":42,\"review_id\":\"R-1\"}\n\n"
        );
        let back: DaemonEvent =
            serde_json::from_str(f.lines().nth(2).unwrap().trim_start_matches("data: ")).unwrap();
        assert_eq!(back, e);
    }
}
