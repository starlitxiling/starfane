//! SSE push events (contracts §1.9).
//!
//! Frame: `id: <seq>\nevent: <type>\ndata: <json>\n\n`.

use crate::api::{AttentionItem, DeliveryReport, DispatchTarget};
use crate::event::NormalizedEvent;
use crate::id::{ArtifactId, PipelineId, ReviewId, SessionId};
use crate::pipeline::{ArtifactKind, PipelineSpec};
use crate::session::{HostRecord, SessionMetrics, SessionRecord};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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
    Notifiers {
        seq: u64,
        #[serde(default)]
        global_mute_until_ms: Option<u64>,
        #[serde(default)]
        channel_mutes: BTreeMap<String, u64>,
    },
    Output {
        seq: u64,
        session_id: SessionId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        start_seq: Option<usize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        total_messages: Option<usize>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        appended_messages: Vec<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        live_tail: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metrics: Option<SessionMetrics>,
        #[serde(default)]
        dirty: bool,
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
            | DaemonEvent::Host { seq, .. }
            | DaemonEvent::Notifiers { seq, .. }
            | DaemonEvent::Output { seq, .. } => *seq,
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
            DaemonEvent::Notifiers { .. } => "notifiers",
            DaemonEvent::Output { .. } => "output",
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

        let mut mutes = BTreeMap::new();
        mutes.insert("tg-ops".to_string(), 1_764_054_000_000);
        let ne = DaemonEvent::Notifiers {
            seq: 43,
            global_mute_until_ms: None,
            channel_mutes: mutes,
        };
        let nf = ne.to_sse_frame().unwrap();
        assert!(nf.starts_with("id: 43\nevent: notifiers\ndata: "));
        let nback: DaemonEvent =
            serde_json::from_str(nf.lines().nth(2).unwrap().trim_start_matches("data: ")).unwrap();
        assert_eq!(nback, ne);

        let oe = DaemonEvent::Output {
            seq: 44,
            session_id: SessionId::new("mac-xiling:tmux-%39"),
            start_seq: Some(25),
            total_messages: Some(26),
            appended_messages: vec![serde_json::json!({"role": "assistant", "text": "ok"})],
            live_tail: Some("thinking...".into()),
            metrics: Some(SessionMetrics {
                context_used_tokens: Some(12_000),
                context_window_tokens: Some(200_000),
                context_used_pct: Some(6.0),
                tps: Some(42.5),
                ttft_ms: None,
                avg_ttft_ms: None,
                last_output_tokens: Some(40),
                last_duration_ms: Some(900),
                total_input_tokens: None,
                total_output_tokens: None,
                total_cache_read_tokens: None,
                recent_turns: Vec::new(),
                source: "transcript".into(),
                updated_ms: 1_700_000_000_000,
            }),
            dirty: false,
        };
        let of = oe.to_sse_frame().unwrap();
        assert!(of.starts_with("id: 44\nevent: output\ndata: "));
        let ojson: serde_json::Value =
            serde_json::from_str(of.lines().nth(2).unwrap().trim_start_matches("data: ")).unwrap();
        assert_eq!(ojson["type"], "output");
        assert_eq!(ojson["session_id"], "mac-xiling:tmux-%39");
        assert_eq!(ojson["start_seq"], 25);
        assert_eq!(ojson["total_messages"], 26);
        assert_eq!(ojson["dirty"], false);
        let oback: DaemonEvent = serde_json::from_value(ojson).unwrap();
        assert_eq!(oback, oe);

        let dirty_only = DaemonEvent::Output {
            seq: 45,
            session_id: SessionId::new("mac-xiling:tmux-%39"),
            start_seq: None,
            total_messages: None,
            appended_messages: Vec::new(),
            live_tail: None,
            metrics: None,
            dirty: true,
        };
        let df = dirty_only.to_sse_frame().unwrap();
        let djson: serde_json::Value =
            serde_json::from_str(df.lines().nth(2).unwrap().trim_start_matches("data: ")).unwrap();
        assert!(djson.get("start_seq").is_none());
        assert!(djson.get("total_messages").is_none());
        assert!(djson.get("appended_messages").is_none());
        assert!(djson.get("live_tail").is_none());
        assert!(djson.get("metrics").is_none());
        assert_eq!(djson["dirty"], true);
        let dback: DaemonEvent = serde_json::from_value(djson).unwrap();
        assert_eq!(dback, dirty_only);
    }
}
