//! MCP Event Capture and Management
//!
//! This module provides structures and functionality for capturing, managing,
//! and displaying MCP (Model Context Protocol) events in the TUI interface.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// Status of an MCP event
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum McpEventStatus {
    /// Event is pending/starting
    Pending,
    /// Event completed successfully
    Success,
    /// Event failed
    Failure,
    /// Event was cancelled
    Cancelled,
}

impl McpEventStatus {
}

/// A single MCP event
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpEvent {
    /// Unique event ID
    pub id: String,
    /// Provider name
    pub provider: String,
    /// Method/tool name
    pub method: String,
    /// Event status
    pub status: McpEventStatus,
    /// Arguments preview (for debugging)
    pub args_preview: Option<String>,
    /// Full event data (only shown in full mode)
    pub full_data: Option<serde_json::Value>,
    /// Timestamp when event occurred
    pub timestamp: std::time::SystemTime,
    /// Duration in milliseconds (if completed)
    pub duration_ms: Option<u64>,
}

impl McpEvent {
    /// Create a new MCP event
    pub fn new(provider: String, method: String, args_preview: Option<String>) -> Self {
        Self {
            id: format!("mcp_{}_{}", provider, method),
            provider,
            method,
            status: McpEventStatus::Pending,
            args_preview,
            full_data: None,
            timestamp: std::time::SystemTime::now(),
            duration_ms: None,
        }
    }

    /// Mark event as successful
    pub fn success(&mut self, full_data: Option<serde_json::Value>) {
        self.status = McpEventStatus::Success;
        self.full_data = full_data;
        self.update_duration();
    }

    /// Mark event as failed
    pub fn failure(&mut self, error_message: Option<String>) {
        self.status = McpEventStatus::Failure;
        if let Some(error) = error_message {
            self.full_data = Some(serde_json::json!({"error": error}));
        }
        self.update_duration();
    }

    /// Update the duration for this event
    fn update_duration(&mut self) {
        if let Ok(duration) = self.timestamp.elapsed() {
            self.duration_ms = Some(duration.as_millis() as u64);
        }
    }
}

/// MCP panel state for managing events and UI
#[derive(Debug, Clone)]
pub struct McpPanelState {
    /// Event queue (newest first)
    events: VecDeque<McpEvent>,
    /// UI configuration
    /// Maximum number of events to keep
    max_events: usize,
    /// Whether MCP is enabled
    enabled: bool,
}

impl McpPanelState {
    /// Create a new MCP panel state
    pub fn new(max_events: usize) -> Self {
        Self {
            events: VecDeque::new(),
            max_events,
            enabled: true,
        }
    }

    /// Add a new event to the panel
    pub fn add_event(&mut self, event: McpEvent) {
        if !self.enabled {
            return;
        }

        // If we have a pending event with the same provider/method, update it
        if let Some(pending_event) = self.events.iter_mut()
            .find(|e| e.provider == event.provider
                  && e.method == event.method
                  && e.status == McpEventStatus::Pending) {
            pending_event.status = event.status;
            pending_event.args_preview = event.args_preview;
            pending_event.full_data = event.full_data;
            pending_event.update_duration();
            return;
        }

        // Add new event
        self.events.push_front(event);

        // Remove old events if we exceed the limit
        while self.events.len() > self.max_events {
            self.events.pop_back();
        }
    }

}

impl Default for McpPanelState {
    fn default() -> Self {
        Self::new(50)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mcp_event_creation() {
        let event = McpEvent::new("test_provider".to_string(), "test_method".to_string(), Some("test args".to_string()));

        assert_eq!(event.provider, "test_provider");
        assert_eq!(event.method, "test_method");
        assert_eq!(event.status, McpEventStatus::Pending);
        assert_eq!(event.args_preview, Some("test args".to_string()));
        assert!(event.duration_ms.is_none());
    }

    #[test]
    fn test_mcp_event_status_transitions() {
        let mut event = McpEvent::new("test".to_string(), "method".to_string(), None);

        event.success(Some(serde_json::json!({"result": "ok"})));
        assert_eq!(event.status, McpEventStatus::Success);
        assert!(event.duration_ms.is_some());

        let mut event2 = McpEvent::new("test".to_string(), "method".to_string(), None);
        event2.failure(Some("error message".to_string()));
        assert_eq!(event2.status, McpEventStatus::Failure);
    }

    #[test]
    fn test_mcp_panel_state() {
        let ui_config = McpUiConfig {
            mode: McpUiMode::Compact,
            max_events: 10,
            show_provider_names: true,
        };

        let mut panel = McpPanelState::new(5);

        assert!(panel.enabled);
        assert_eq!(panel.event_count(), 0);

        let event = McpEvent::new("provider".to_string(), "method".to_string(), None);
        panel.add_event(event);

        assert_eq!(panel.event_count(), 1);
        assert_eq!(panel.compact_status(), Some("[~] MCP provider `method`".to_string()));
    }

    #[test]
    fn test_mcp_panel_state_disabled() {
        let panel = McpPanelState::disabled();
        assert!(!panel.enabled);
        assert_eq!(panel.max_events, 0);
        assert_eq!(panel.event_count(), 0);
    }

    #[test]
    fn test_event_display_titles() {
        let mut event = McpEvent::new("time".to_string(), "get_current_time".to_string(), None);
        event.success(Some(serde_json::json!({"time": "12:00"})));

        assert_eq!(
            event.compact_title(),
            "[✓] MCP time `get_current_time`"
        );

        let detailed = event.detailed_title();
        assert!(detailed.contains("[✓]"));
        assert!(detailed.contains("get_current_time"));
        assert!(detailed.contains("time"));
        assert!(detailed.ends_with(')')); // Should have duration
    }
}
