use anyhow::Result;
use chrono::Local;
use serde_json::{Map, Value};
use std::time::Duration;
use vtcode_core::ui::slash::SLASH_COMMANDS;
use vtcode_core::ui::theme;
use vtcode_core::utils::ansi::{AnsiRenderer, MessageStyle};
use vtcode_core::utils::session_archive;

pub enum SlashCommandOutcome {
    Handled,
    ThemeChanged(String),
    #[allow(dead_code)]
    ExecuteTool { name: String, args: Value },
    Exit,
}

pub fn handle_slash_command(
    input: &str,
    renderer: &mut AnsiRenderer,
) -> Result<SlashCommandOutcome> {
    let mut parts = input.split_whitespace();
    let command = parts.next().unwrap_or("").to_lowercase();
    if command.is_empty() {
        return Ok(SlashCommandOutcome::Handled);
    }

    match command.as_str() {
        "theme" => {
            let Some(next_theme) = parts.next() else {
                renderer.line(MessageStyle::Error, "Usage: /theme <theme-id>")?;
                return Ok(SlashCommandOutcome::Handled);
            };
            let desired = next_theme.to_lowercase();
            match theme::set_active_theme(&desired) {
                Ok(()) => {
                    let label = theme::active_theme_label();
                    renderer.line(MessageStyle::Info, &format!("Theme switched to {}", label))?;
                    return Ok(SlashCommandOutcome::ThemeChanged(theme::active_theme_id()));
                }
                Err(err) => {
                    renderer.line(
                        MessageStyle::Error,
                        &format!("Theme '{}' not available: {}", next_theme, err),
                    )?;
                }
            }
            Ok(SlashCommandOutcome::Handled)
        }
        "help" => {
            renderer.line(MessageStyle::Info, "Available commands:")?;
            for info in SLASH_COMMANDS.iter() {
                renderer.line(
                    MessageStyle::Info,
                    &format!("  /{} - {}", info.name, info.description),
                )?;
            }
            renderer.line(
                MessageStyle::Info,
                &format!(
                    "  Themes available: {}",
                    theme::available_themes().join(", ")
                ),
            )?;
            Ok(SlashCommandOutcome::Handled)
        }
        "list-themes" => {
            renderer.line(MessageStyle::Info, "Available themes:")?;
            for id in theme::available_themes() {
                let marker = if theme::active_theme_id() == id {
                    "*"
                } else {
                    " "
                };
                let label = theme::theme_label(id).unwrap_or(id);
                renderer.line(
                    MessageStyle::Info,
                    &format!("{} {} ({})", marker, id, label),
                )?;
            }
            Ok(SlashCommandOutcome::Handled)
        }
        "command" => {
            let program = parts.next();
            if program.is_none() {
                renderer.line(MessageStyle::Error, "Usage: /command <program> [args...]")?;
                return Ok(SlashCommandOutcome::Handled);
            }
            let mut command_vec = Vec::new();
            command_vec.push(Value::String(program.unwrap().to_string()));
            command_vec.extend(parts.map(|segment| Value::String(segment.to_string())));

            let mut args_map = Map::new();
            args_map.insert("command".to_string(), Value::Array(command_vec));
            Ok(SlashCommandOutcome::ExecuteTool {
                name: "run_terminal_cmd".to_string(),
                args: Value::Object(args_map),
            })
        }
        "sessions" => {
            let limit = parts
                .next()
                .and_then(|value| value.parse::<usize>().ok())
                .map(|value| value.clamp(1, 25))
                .unwrap_or(5);

            match session_archive::list_recent_sessions(limit) {
                Ok(listings) => {
                    if listings.is_empty() {
                        renderer.line(MessageStyle::Info, "No archived sessions found.")?;
                    } else {
                        renderer.line(MessageStyle::Info, "Recent sessions:")?;
                        for (index, listing) in listings.iter().enumerate() {
                            if index > 0 {
                                renderer.line(MessageStyle::Info, "")?;
                            }

                            let ended_local = listing
                                .snapshot
                                .ended_at
                                .with_timezone(&Local)
                                .format("%Y-%m-%d %H:%M");
                            let duration = listing
                                .snapshot
                                .ended_at
                                .signed_duration_since(listing.snapshot.started_at);
                            let duration_std =
                                duration.to_std().unwrap_or_else(|_| Duration::from_secs(0));
                            let duration_label = format_duration_label(duration_std);
                            let tool_count = listing.snapshot.distinct_tools.len();
                            let header = format!(
                                "- (ID: {}) {} · Model: {} · Workspace: {}",
                                listing.identifier(),
                                ended_local,
                                listing.snapshot.metadata.model,
                                listing.snapshot.metadata.workspace_label,
                            );
                            renderer.line(MessageStyle::Info, &header)?;

                            let detail = format!(
                                "    Duration: {} · {} msgs · {} tools",
                                duration_label, listing.snapshot.total_messages, tool_count,
                            );
                            renderer.line(MessageStyle::Info, &detail)?;

                            if let Some(prompt) = listing.first_prompt_preview() {
                                renderer
                                    .line(MessageStyle::Info, &format!("    Prompt: {prompt}"))?;
                            }

                            if let Some(reply) = listing.first_reply_preview() {
                                renderer
                                    .line(MessageStyle::Info, &format!("    Reply: {reply}"))?;
                            }

                            renderer.line(
                                MessageStyle::Info,
                                &format!("    File: {}", listing.path.display()),
                            )?;
                        }
                    }
                }
                Err(err) => {
                    renderer.line(
                        MessageStyle::Error,
                        &format!("Failed to load session archives: {}", err),
                    )?;
                }
            }
            Ok(SlashCommandOutcome::Handled)
        }
        "exit" => Ok(SlashCommandOutcome::Exit),
        _ => {
            renderer.line(
                MessageStyle::Error,
                &format!("Unknown command '/{}'. Try /help.", command),
            )?;
            Ok(SlashCommandOutcome::Handled)
        }
    }
}

fn format_duration_label(duration: Duration) -> String {
    let total_seconds = duration.as_secs();
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;

    let mut parts = Vec::new();
    if hours > 0 {
        parts.push(format!("{}h", hours));
    }
    if minutes > 0 || hours > 0 {
        parts.push(format!("{}m", minutes));
    }
    parts.push(format!("{}s", seconds));
    parts.join(" ")
}
