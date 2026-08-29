//! Chat transcript model: converts the control plane's reconstructed
//! history items and live pi RPC events into renderable chat items, and
//! renders them with egui.

use egui::{Color32, RichText, Ui};
use serde_json::Value;

#[derive(Debug, Clone)]
pub enum Part {
    Text(String),
    Thinking(String),
    ToolCall { name: String, args: String },
}

#[derive(Debug, Clone)]
pub enum ChatItem {
    User(String),
    Assistant(Vec<Part>),
    ToolResult {
        name: String,
        text: String,
        is_error: bool,
    },
    /// System lines: session boundaries, wake narration, notices, crashes.
    System(String),
    Error(String),
}

/// Maximum number of retained chat items; older items are dropped once
/// this cap is exceeded, to bound memory/render cost for long sessions.
const MAX_CHAT_ITEMS: usize = 2000;

#[derive(Default)]
pub struct Chat {
    #[allow(dead_code)]
    pub agent: String,
    pub items: Vec<ChatItem>,
    pub input: String,
    pub streaming: bool,
    pub history_done: bool,
    /// Status line under the transcript (agent status, wake narration).
    pub status_line: String,
    /// Set when the session stream closed; shown with a reconnect hint.
    pub closed: Option<Option<String>>,
}

impl Chat {
    pub fn new(agent: String) -> Self {
        Self {
            agent,
            ..Default::default()
        }
    }

    /// Push a chat item, trimming the oldest items once the retention cap
    /// (`MAX_CHAT_ITEMS`) is exceeded.
    fn push_item(&mut self, item: ChatItem) {
        self.items.push(item);
        if self.items.len() > MAX_CHAT_ITEMS {
            let excess = self.items.len() - MAX_CHAT_ITEMS;
            self.items.drain(0..excess);
        }
    }

    /// A replayed history item ({role, parts}) from the server.
    pub fn push_history(&mut self, item: &Value) {
        let role = item["role"].as_str().unwrap_or("");
        let parts = item["parts"].as_array();
        match role {
            "user" => {
                let text = parts_text(parts);
                if !text.trim().is_empty() {
                    self.push_item(ChatItem::User(text));
                }
            }
            "assistant" => {
                self.push_item(ChatItem::Assistant(assistant_parts(parts)));
            }
            "toolResult" => {
                if let Some(tr) = tool_result_part(parts) {
                    self.push_item(tr);
                }
            }
            "system" => self.push_item(ChatItem::System(parts_text(parts))),
            _ => {}
        }
    }

    /// A live event (raw pi RPC event or synthetic status/notice).
    pub fn push_live(&mut self, event: &Value) {
        match event["type"].as_str().unwrap_or("") {
            "turn_start" => self.streaming = true,
            "turn_end" | "agent_end" | "agent_settled" => self.streaming = false,
            "status" | "notice" => {
                let msg = event["message"].as_str().unwrap_or("").to_string();
                if let Some(status) = event["status"].as_str() {
                    self.status_line = format!("{status} — {msg}");
                }
                if !msg.is_empty() {
                    self.push_item(ChatItem::System(msg));
                }
            }
            "message_end" => {
                if let Some(item) = message_to_item(&event["message"]) {
                    self.push_item(item);
                }
            }
            "tool_execution_start" => {
                let name = event["toolName"].as_str().unwrap_or("tool");
                self.status_line = format!("running {name}…");
            }
            "tool_execution_end" => self.status_line.clear(),
            _ => {}
        }
    }
}

fn parts_text(parts: Option<&Vec<Value>>) -> String {
    parts
        .map(|ps| {
            ps.iter()
                .filter(|p| p["type"] == "text" || p["type"] == "session_boundary")
                .filter_map(|p| p["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn assistant_parts(parts: Option<&Vec<Value>>) -> Vec<Part> {
    let mut out = Vec::new();
    for p in parts.cloned().unwrap_or_default() {
        match p["type"].as_str() {
            Some("text") => out.push(Part::Text(
                p["text"].as_str().unwrap_or_default().to_string(),
            )),
            Some("thinking") => out.push(Part::Thinking(
                p["text"].as_str().unwrap_or_default().to_string(),
            )),
            Some("tool_call") => out.push(Part::ToolCall {
                name: p["name"].as_str().unwrap_or("tool").to_string(),
                args: summarize_args(&p["arguments"]),
            }),
            Some("error") => out.push(Part::Text(format!(
                "⚠ {}",
                p["text"].as_str().unwrap_or("turn failed")
            ))),
            _ => {}
        }
    }
    out
}

fn tool_result_part(parts: Option<&Vec<Value>>) -> Option<ChatItem> {
    let p = parts?.first()?;
    Some(ChatItem::ToolResult {
        name: p["name"].as_str().unwrap_or("tool").to_string(),
        text: p["text"].as_str().unwrap_or_default().to_string(),
        is_error: p["is_error"].as_bool().unwrap_or(false),
    })
}

/// Convert a raw pi `message_end` message (live path) into a chat item.
/// Mirrors the server's `transcript_item` (crates/suzerain/src/web_session.rs).
pub fn message_to_item(message: &Value) -> Option<ChatItem> {
    let role = message["role"].as_str()?;
    match role {
        "assistant" => {
            let mut parts = Vec::new();
            for c in message["content"].as_array()?.iter() {
                match c["type"].as_str() {
                    Some("text") => parts.push(Part::Text(
                        c["text"].as_str().unwrap_or_default().to_string(),
                    )),
                    Some("thinking") => parts.push(Part::Thinking(
                        c["thinking"].as_str().unwrap_or_default().to_string(),
                    )),
                    Some("toolCall") => parts.push(Part::ToolCall {
                        name: c["name"].as_str().unwrap_or("tool").to_string(),
                        args: summarize_args(&c["arguments"]),
                    }),
                    _ => {}
                }
            }
            if matches!(
                message["stopReason"].as_str(),
                Some("error") | Some("aborted")
            ) {
                let detail = message["errorMessage"].as_str().unwrap_or("");
                return Some(ChatItem::Error(if detail.is_empty() {
                    format!(
                        "turn ended: {}",
                        message["stopReason"].as_str().unwrap_or("error")
                    )
                } else {
                    format!("LLM request failed: {detail}")
                }));
            }
            if parts.is_empty() {
                None
            } else {
                Some(ChatItem::Assistant(parts))
            }
        }
        "toolResult" => {
            let text: String = message["content"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter(|c| c["type"] == "text")
                        .filter_map(|c| c["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
            Some(ChatItem::ToolResult {
                name: message["toolName"].as_str().unwrap_or("tool").to_string(),
                text,
                is_error: message["isError"].as_bool().unwrap_or(false),
            })
        }
        _ => {
            let text = match &message["content"] {
                Value::String(s) => s.clone(),
                Value::Array(arr) => arr
                    .iter()
                    .filter(|c| c["type"] == "text")
                    .filter_map(|c| c["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => String::new(),
            };
            if text.trim().is_empty() {
                None
            } else {
                Some(ChatItem::User(text))
            }
        }
    }
}

fn summarize_args(args: &Value) -> String {
    let s = match args {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let one_line = s.replace('\n', " ");
    const MAX: usize = 200;
    if one_line.chars().count() > MAX {
        let truncated: String = one_line.chars().take(MAX).collect();
        format!("{truncated}…")
    } else {
        one_line
    }
}

// ── rendering ────────────────────────────────────────────────────────────

const USER_BG: Color32 = crate::theme::USER_BUBBLE;
const ASSISTANT_BG: Color32 = crate::theme::ASSISTANT_BUBBLE;
const ERROR_RED: Color32 = crate::theme::ERROR;
const SYSTEM_GRAY: Color32 = crate::theme::SYSTEM_TEXT;

pub fn render_items(ui: &mut Ui, items: &[ChatItem]) {
    for (i, item) in items.iter().enumerate() {
        match item {
            ChatItem::User(text) => {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
                    bubble(ui, USER_BG, |ui| {
                        ui.label(RichText::new(text).color(Color32::WHITE));
                    });
                });
            }
            ChatItem::Assistant(parts) => {
                bubble(ui, ASSISTANT_BG, |ui| {
                    for (j, part) in parts.iter().enumerate() {
                        match part {
                            Part::Text(t) => {
                                ui.label(RichText::new(t).color(Color32::LIGHT_GRAY));
                            }
                            Part::Thinking(t) => {
                                egui::CollapsingHeader::new(
                                    RichText::new("💭 thinking").italics().color(SYSTEM_GRAY),
                                )
                                .id_salt(ui.id().with(("think", i, j)))
                                .show(ui, |ui| {
                                    ui.label(RichText::new(t).italics().color(SYSTEM_GRAY));
                                });
                            }
                            Part::ToolCall { name, args } => {
                                egui::CollapsingHeader::new(
                                    RichText::new(format!("🔧 {name}")).color(Color32::KHAKI),
                                )
                                .id_salt(ui.id().with(("tool", i, j)))
                                .show(ui, |ui| {
                                    ui.label(
                                        RichText::new(args)
                                            .monospace()
                                            .color(SYSTEM_GRAY)
                                            .size(11.0),
                                    );
                                });
                            }
                        }
                    }
                });
            }
            ChatItem::ToolResult {
                name,
                text,
                is_error,
            } => {
                let color = if *is_error { ERROR_RED } else { SYSTEM_GRAY };
                egui::CollapsingHeader::new(
                    RichText::new(format!("↳ {name} result"))
                        .color(color)
                        .size(12.0),
                )
                .id_salt(ui.id().with(("result", i)))
                .show(ui, |ui| {
                    let short: String = text.chars().take(2000).collect();
                    ui.label(RichText::new(short).monospace().size(11.0).color(color));
                });
            }
            ChatItem::System(text) => {
                ui.label(RichText::new(text).italics().color(SYSTEM_GRAY).size(11.5));
            }
            ChatItem::Error(text) => {
                ui.label(RichText::new(text).color(ERROR_RED));
            }
        }
        ui.add_space(4.0);
    }
}

fn bubble(ui: &mut Ui, bg: Color32, add: impl FnOnce(&mut Ui)) {
    crate::theme::bubble_frame(bg).show(ui, |ui| {
        ui.set_max_width(ui.available_width() * 0.92);
        add(ui);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_items_are_capped_and_drop_oldest_first() {
        let mut chat = Chat::new("test-agent".to_string());
        let total = MAX_CHAT_ITEMS + 500;
        for i in 0..total {
            chat.push_live(&serde_json::json!({
                "type": "status",
                "status": "ok",
                "message": format!("item-{i}"),
            }));
        }
        assert_eq!(chat.items.len(), MAX_CHAT_ITEMS);
        // The oldest items (item-0 .. item-499) should have been dropped;
        // the most recently pushed item should still be present.
        match chat.items.last() {
            Some(ChatItem::System(text)) => {
                assert_eq!(text, &format!("item-{}", total - 1));
            }
            other => panic!("expected last item to be System, got {other:?}"),
        }
        match chat.items.first() {
            Some(ChatItem::System(text)) => {
                assert_eq!(text, "item-500");
            }
            other => panic!("expected first item to be System(item-500), got {other:?}"),
        }
    }
}
