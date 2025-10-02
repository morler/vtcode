use anstyle::{Color as AnsiColorEnum, Style as AnsiStyle};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::config::constants::ui;

#[derive(Clone)]
pub struct InlineHeaderContext {
    pub version: String,
    pub mode: String,
    pub reasoning: String,
    pub workspace_trust: String,
    pub tools: String,
    pub languages: String,
    pub mcp: String,
}

impl Default for InlineHeaderContext {
    fn default() -> Self {
        let version = env!("CARGO_PKG_VERSION").to_string();
        let reasoning = format!(
            "{}{}",
            ui::HEADER_REASONING_PREFIX,
            ui::HEADER_UNKNOWN_PLACEHOLDER
        );
        let trust = format!(
            "{}{}",
            ui::HEADER_TRUST_PREFIX,
            ui::HEADER_UNKNOWN_PLACEHOLDER
        );
        let tools = format!(
            "{}{}",
            ui::HEADER_TOOLS_PREFIX,
            ui::HEADER_UNKNOWN_PLACEHOLDER
        );
        let languages = format!(
            "{}{}",
            ui::HEADER_LANGUAGES_PREFIX,
            ui::HEADER_UNKNOWN_PLACEHOLDER
        );
        let mcp = format!(
            "{}{}",
            ui::HEADER_MCP_PREFIX,
            ui::HEADER_UNKNOWN_PLACEHOLDER
        );

        Self {
            version,
            mode: ui::HEADER_MODE_INLINE.to_string(),
            reasoning,
            workspace_trust: trust,
            tools,
            languages,
            mcp,
        }
    }
}

#[derive(Clone, Default, PartialEq)]
pub struct InlineTextStyle {
    pub color: Option<AnsiColorEnum>,
    pub bold: bool,
    pub italic: bool,
}

impl InlineTextStyle {
    #[must_use]
    pub fn merge_color(mut self, fallback: Option<AnsiColorEnum>) -> Self {
        if self.color.is_none() {
            self.color = fallback;
        }
        self
    }

    #[must_use]
    pub fn to_ansi_style(&self, fallback: Option<AnsiColorEnum>) -> AnsiStyle {
        let mut style = AnsiStyle::new();
        if let Some(color) = self.color.or(fallback) {
            style = style.fg_color(Some(color));
        }
        if self.bold {
            style = style.bold();
        }
        if self.italic {
            style = style.italic();
        }
        style
    }
}

#[derive(Clone, Default)]
pub struct InlineSegment {
    pub text: String,
    pub style: InlineTextStyle,
}

#[derive(Clone, Default)]
pub struct InlineTheme {
    pub foreground: Option<AnsiColorEnum>,
    pub primary: Option<AnsiColorEnum>,
    pub secondary: Option<AnsiColorEnum>,
    pub tool_accent: Option<AnsiColorEnum>,
    pub tool_body: Option<AnsiColorEnum>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InlineMessageKind {
    Agent,
    Error,
    Info,
    Policy,
    Pty,
    Tool,
    User,
}

pub enum InlineCommand {
    AppendLine {
        kind: InlineMessageKind,
        segments: Vec<InlineSegment>,
    },
    Inline {
        kind: InlineMessageKind,
        segment: InlineSegment,
    },
    ReplaceLast {
        count: usize,
        kind: InlineMessageKind,
        lines: Vec<Vec<InlineSegment>>,
    },
    SetPrompt {
        prefix: String,
        style: InlineTextStyle,
    },
    SetPlaceholder {
        hint: Option<String>,
        style: Option<InlineTextStyle>,
    },
    SetMessageLabels {
        agent: Option<String>,
        user: Option<String>,
    },
    SetHeaderContext {
        context: InlineHeaderContext,
    },
    SetTheme {
        theme: InlineTheme,
    },
    SetCursorVisible(bool),
    SetInputEnabled(bool),
    SetInput(String),
    ClearInput,
    ForceRedraw,
    ShowModal {
        title: String,
        lines: Vec<String>,
    },
    CloseModal,
    Shutdown,
}

#[derive(Debug, Clone)]
pub enum InlineEvent {
    Submit(String),
    Cancel,
    Exit,
    Interrupt,
    ScrollLineUp,
    ScrollLineDown,
    ScrollPageUp,
    ScrollPageDown,
}

#[derive(Clone)]
pub struct InlineHandle {
    pub(crate) sender: UnboundedSender<InlineCommand>,
}

impl InlineHandle {
    pub fn append_line(&self, kind: InlineMessageKind, segments: Vec<InlineSegment>) {
        let segments = if segments.is_empty() {
            vec![InlineSegment::default()]
        } else {
            segments
        };
        let _ = self
            .sender
            .send(InlineCommand::AppendLine { kind, segments });
    }

    pub fn inline(&self, kind: InlineMessageKind, segment: InlineSegment) {
        let _ = self.sender.send(InlineCommand::Inline { kind, segment });
    }

    pub fn replace_last(
        &self,
        count: usize,
        kind: InlineMessageKind,
        lines: Vec<Vec<InlineSegment>>,
    ) {
        let _ = self
            .sender
            .send(InlineCommand::ReplaceLast { count, kind, lines });
    }

    pub fn set_prompt(&self, prefix: String, style: InlineTextStyle) {
        let _ = self.sender.send(InlineCommand::SetPrompt { prefix, style });
    }

    pub fn set_placeholder(&self, hint: Option<String>) {
        self.set_placeholder_with_style(hint, None);
    }

    pub fn set_placeholder_with_style(&self, hint: Option<String>, style: Option<InlineTextStyle>) {
        let _ = self
            .sender
            .send(InlineCommand::SetPlaceholder { hint, style });
    }

    pub fn set_message_labels(&self, agent: Option<String>, user: Option<String>) {
        let _ = self
            .sender
            .send(InlineCommand::SetMessageLabels { agent, user });
    }

    pub fn set_header_context(&self, context: InlineHeaderContext) {
        let _ = self
            .sender
            .send(InlineCommand::SetHeaderContext { context });
    }

    pub fn set_theme(&self, theme: InlineTheme) {
        let _ = self.sender.send(InlineCommand::SetTheme { theme });
    }

    pub fn set_cursor_visible(&self, visible: bool) {
        let _ = self.sender.send(InlineCommand::SetCursorVisible(visible));
    }

    pub fn set_input_enabled(&self, enabled: bool) {
        let _ = self.sender.send(InlineCommand::SetInputEnabled(enabled));
    }

    pub fn set_input(&self, content: String) {
        let _ = self.sender.send(InlineCommand::SetInput(content));
    }

    pub fn clear_input(&self) {
        let _ = self.sender.send(InlineCommand::ClearInput);
    }

    pub fn force_redraw(&self) {
        let _ = self.sender.send(InlineCommand::ForceRedraw);
    }

    pub fn shutdown(&self) {
        let _ = self.sender.send(InlineCommand::Shutdown);
    }

    pub fn show_modal(&self, title: String, lines: Vec<String>) {
        let _ = self.sender.send(InlineCommand::ShowModal { title, lines });
    }

    pub fn close_modal(&self) {
        let _ = self.sender.send(InlineCommand::CloseModal);
    }
}

pub struct InlineSession {
    pub handle: InlineHandle,
    pub events: UnboundedReceiver<InlineEvent>,
}
