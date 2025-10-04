use crate::config::constants::{models, urls};
use crate::config::core::{OpenRouterPromptCacheSettings, PromptCachingConfig};
use crate::llm::client::LLMClient;
use crate::llm::error_display;
use crate::llm::provider::{
    FinishReason, LLMError, LLMProvider, LLMRequest, LLMResponse, LLMStream, LLMStreamEvent,
    Message, MessageRole, ToolCall, ToolChoice, ToolDefinition, Usage,
};
use crate::llm::types as llm_types;
use async_stream::try_stream;
use async_trait::async_trait;
use futures::StreamExt;
use reqwest::{Client as HttpClient, Response, StatusCode};
use serde_json::{Map, Value, json};

use super::{extract_reasoning_trace, gpt5_codex_developer_prompt};

#[derive(Default, Clone)]
struct ToolCallBuilder {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

impl ToolCallBuilder {
    fn finalize(self, fallback_index: usize) -> Option<ToolCall> {
        let name = self.name?;
        let id = self
            .id
            .unwrap_or_else(|| format!("tool_call_{}", fallback_index));
        let arguments = if self.arguments.is_empty() {
            "{}".to_string()
        } else {
            self.arguments
        };
        Some(ToolCall::function(id, name, arguments))
    }
}

fn update_tool_calls(builders: &mut Vec<ToolCallBuilder>, deltas: &[Value]) {
    for (index, delta) in deltas.iter().enumerate() {
        if builders.len() <= index {
            builders.push(ToolCallBuilder::default());
        }
        let builder = builders
            .get_mut(index)
            .expect("tool call builder must exist after push");

        if let Some(id) = delta.get("id").and_then(|v| v.as_str()) {
            builder.id = Some(id.to_string());
        }

        if let Some(function) = delta.get("function") {
            if let Some(name) = function.get("name").and_then(|v| v.as_str()) {
                builder.name = Some(name.to_string());
            }

            if let Some(arguments_value) = function.get("arguments") {
                if let Some(arguments) = arguments_value.as_str() {
                    builder.arguments.push_str(arguments);
                } else if arguments_value.is_object() || arguments_value.is_array() {
                    builder.arguments.push_str(&arguments_value.to_string());
                }
            }
        }
    }
}

fn finalize_tool_calls(builders: Vec<ToolCallBuilder>) -> Option<Vec<ToolCall>> {
    let calls: Vec<ToolCall> = builders
        .into_iter()
        .enumerate()
        .filter_map(|(index, builder)| builder.finalize(index))
        .collect();

    if calls.is_empty() { None } else { Some(calls) }
}

#[derive(Debug, PartialEq, Eq)]
enum StreamFragment {
    Content(String),
    Reasoning(String),
}

#[derive(Default, Debug)]
struct StreamDelta {
    fragments: Vec<StreamFragment>,
}

impl StreamDelta {
    fn push_content(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }

        match self.fragments.last_mut() {
            Some(StreamFragment::Content(existing)) => existing.push_str(text),
            _ => self
                .fragments
                .push(StreamFragment::Content(text.to_string())),
        }
    }

    fn push_reasoning(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }

        match self.fragments.last_mut() {
            Some(StreamFragment::Reasoning(existing)) => existing.push_str(text),
            _ => self
                .fragments
                .push(StreamFragment::Reasoning(text.to_string())),
        }
    }

    fn is_empty(&self) -> bool {
        self.fragments.is_empty()
    }

    fn into_fragments(self) -> Vec<StreamFragment> {
        self.fragments
    }

    fn extend(&mut self, other: StreamDelta) {
        self.fragments.extend(other.fragments);
    }
}

#[derive(Default, Clone)]
struct ReasoningBuffer {
    text: String,
    last_chunk: Option<String>,
}

impl ReasoningBuffer {
    fn push(&mut self, chunk: &str) -> Option<String> {
        if chunk.trim().is_empty() {
            return None;
        }

        let normalized = Self::normalize_chunk(chunk);

        if normalized.is_empty() {
            return None;
        }

        if self.last_chunk.as_deref() == Some(&normalized) {
            return None;
        }

        let last_has_spacing = self.text.ends_with(' ') || self.text.ends_with('\n');
        let chunk_starts_with_space = chunk
            .chars()
            .next()
            .map(|value| value.is_whitespace())
            .unwrap_or(false);
        let leading_punctuation = Self::is_leading_punctuation(chunk);
        let trailing_connector = Self::ends_with_connector(&self.text);

        let mut delta = String::new();

        if !self.text.is_empty()
            && !last_has_spacing
            && !chunk_starts_with_space
            && !leading_punctuation
            && !trailing_connector
        {
            delta.push(' ');
        }

        delta.push_str(&normalized);
        self.text.push_str(&delta);
        self.last_chunk = Some(normalized);

        Some(delta)
    }

    fn finalize(self) -> Option<String> {
        let trimmed = self.text.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    }

    fn normalize_chunk(chunk: &str) -> String {
        let mut normalized = String::new();
        for part in chunk.split_whitespace() {
            if !normalized.is_empty() {
                normalized.push(' ');
            }
            normalized.push_str(part);
        }
        normalized
    }

    fn is_leading_punctuation(chunk: &str) -> bool {
        chunk
            .chars()
            .find(|ch| !ch.is_whitespace())
            .map(|ch| matches!(ch, ',' | '.' | '!' | '?' | ':' | ';' | ')' | ']' | '}'))
            .unwrap_or(false)
    }

    fn ends_with_connector(text: &str) -> bool {
        text.chars()
            .rev()
            .find(|ch| !ch.is_whitespace())
            .map(|ch| matches!(ch, '(' | '[' | '{' | '/' | '-'))
            .unwrap_or(false)
    }
}

fn apply_tool_call_delta_from_content(
    builders: &mut Vec<ToolCallBuilder>,
    container: &Map<String, Value>,
) {
    if let Some(nested) = container.get("delta").and_then(|value| value.as_object()) {
        apply_tool_call_delta_from_content(builders, nested);
    }

    let (index, delta_source) = if let Some(tool_call_value) = container.get("tool_call") {
        match tool_call_value.as_object() {
            Some(tool_call) => {
                let idx = tool_call
                    .get("index")
                    .and_then(|value| value.as_u64())
                    .unwrap_or(0) as usize;
                (idx, tool_call)
            }
            None => (0usize, container),
        }
    } else {
        let idx = container
            .get("index")
            .and_then(|value| value.as_u64())
            .unwrap_or(0) as usize;
        (idx, container)
    };

    let mut delta_map = Map::new();

    if let Some(id_value) = delta_source.get("id") {
        delta_map.insert("id".to_string(), id_value.clone());
    }

    if let Some(function_value) = delta_source.get("function") {
        delta_map.insert("function".to_string(), function_value.clone());
    }

    if delta_map.is_empty() {
        return;
    }

    if builders.len() <= index {
        builders.resize_with(index + 1, ToolCallBuilder::default);
    }

    let mut deltas = vec![Value::Null; index + 1];
    deltas[index] = Value::Object(delta_map);
    update_tool_calls(builders, &deltas);
}

fn process_content_object(
    map: &Map<String, Value>,
    aggregated_content: &mut String,
    reasoning: &mut ReasoningBuffer,
    tool_call_builders: &mut Vec<ToolCallBuilder>,
    deltas: &mut StreamDelta,
) {
    if let Some(content_type) = map.get("type").and_then(|value| value.as_str()) {
        match content_type {
            "reasoning" | "thinking" | "analysis" => {
                if let Some(text_value) = map.get("text").and_then(|value| value.as_str()) {
                    if let Some(delta) = reasoning.push(text_value) {
                        deltas.push_reasoning(&delta);
                    }
                } else if let Some(text_value) =
                    map.get("output_text").and_then(|value| value.as_str())
                {
                    if let Some(delta) = reasoning.push(text_value) {
                        deltas.push_reasoning(&delta);
                    }
                }
                return;
            }
            "tool_call_delta" | "tool_call" => {
                apply_tool_call_delta_from_content(tool_call_builders, map);
                return;
            }
            _ => {}
        }
    }

    if let Some(tool_call_value) = map.get("tool_call").and_then(|value| value.as_object()) {
        apply_tool_call_delta_from_content(tool_call_builders, tool_call_value);
        return;
    }

    if let Some(text_value) = map.get("text").and_then(|value| value.as_str()) {
        if !text_value.is_empty() {
            aggregated_content.push_str(text_value);
            deltas.push_content(text_value);
        }
        return;
    }

    if let Some(text_value) = map.get("output_text").and_then(|value| value.as_str()) {
        if !text_value.is_empty() {
            aggregated_content.push_str(text_value);
            deltas.push_content(text_value);
        }
        return;
    }

    if let Some(text_value) = map
        .get("output_text_delta")
        .and_then(|value| value.as_str())
    {
        if !text_value.is_empty() {
            aggregated_content.push_str(text_value);
            deltas.push_content(text_value);
        }
        return;
    }

    for key in ["content", "items", "output", "outputs", "delta"] {
        if let Some(inner) = map.get(key) {
            process_content_value(
                inner,
                aggregated_content,
                reasoning,
                tool_call_builders,
                deltas,
            );
        }
    }
}

fn process_content_part(
    part: &Value,
    aggregated_content: &mut String,
    reasoning: &mut ReasoningBuffer,
    tool_call_builders: &mut Vec<ToolCallBuilder>,
    deltas: &mut StreamDelta,
) {
    if let Some(text) = part.as_str() {
        if !text.is_empty() {
            aggregated_content.push_str(text);
            deltas.push_content(text);
        }
        return;
    }

    if let Some(map) = part.as_object() {
        process_content_object(
            map,
            aggregated_content,
            reasoning,
            tool_call_builders,
            deltas,
        );
        return;
    }

    if part.is_array() {
        process_content_value(
            part,
            aggregated_content,
            reasoning,
            tool_call_builders,
            deltas,
        );
    }
}

fn process_content_value(
    value: &Value,
    aggregated_content: &mut String,
    reasoning: &mut ReasoningBuffer,
    tool_call_builders: &mut Vec<ToolCallBuilder>,
    deltas: &mut StreamDelta,
) {
    match value {
        Value::String(text) => {
            if !text.is_empty() {
                aggregated_content.push_str(text);
                deltas.push_content(text);
            }
        }
        Value::Array(parts) => {
            for part in parts {
                process_content_part(
                    part,
                    aggregated_content,
                    reasoning,
                    tool_call_builders,
                    deltas,
                );
            }
        }
        Value::Object(map) => {
            process_content_object(
                map,
                aggregated_content,
                reasoning,
                tool_call_builders,
                deltas,
            );
        }
        _ => {}
    }
}

fn extract_tool_calls_from_content(message: &Value) -> Option<Vec<ToolCall>> {
    let parts = message.get("content").and_then(|value| value.as_array())?;
    let mut calls: Vec<ToolCall> = Vec::new();

    for (index, part) in parts.iter().enumerate() {
        let map = match part.as_object() {
            Some(value) => value,
            None => continue,
        };

        let content_type = map.get("type").and_then(|value| value.as_str());
        let is_tool_call = matches!(content_type, Some("tool_call") | Some("function_call"))
            || (content_type.is_none()
                && map.contains_key("name")
                && map.contains_key("arguments"));

        if !is_tool_call {
            continue;
        }

        let id = map
            .get("id")
            .and_then(|value| value.as_str())
            .map(|value| value.to_string())
            .unwrap_or_else(|| format!("tool_call_{}", index));

        let (name, arguments_value) =
            if let Some(function) = map.get("function").and_then(|value| value.as_object()) {
                (
                    function
                        .get("name")
                        .and_then(|value| value.as_str())
                        .map(|value| value.to_string()),
                    function.get("arguments"),
                )
            } else {
                (
                    map.get("name")
                        .and_then(|value| value.as_str())
                        .map(|value| value.to_string()),
                    map.get("arguments"),
                )
            };

        let Some(name) = name else {
            continue;
        };

        let arguments = arguments_value
            .map(|value| {
                if let Some(text) = value.as_str() {
                    text.to_string()
                } else if value.is_null() {
                    "{}".to_string()
                } else {
                    value.to_string()
                }
            })
            .unwrap_or_else(|| "{}".to_string());

        calls.push(ToolCall::function(id, name, arguments));
    }

    if calls.is_empty() { None } else { Some(calls) }
}

fn extract_reasoning_from_message_content(message: &Value) -> Option<String> {
    let parts = message.get("content")?.as_array()?;
    let mut segments: Vec<String> = Vec::new();

    for part in parts {
        match part {
            Value::Object(map) => {
                let part_type = map
                    .get("type")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");

                if matches!(part_type, "reasoning" | "thinking" | "analysis") {
                    if let Some(extracted) = extract_reasoning_trace(part) {
                        if !extracted.trim().is_empty() {
                            segments.push(extracted);
                            continue;
                        }
                    }

                    if let Some(text) = map.get("text").and_then(|value| value.as_str()) {
                        let trimmed = text.trim();
                        if !trimmed.is_empty() {
                            segments.push(trimmed.to_string());
                        }
                    }
                }
            }
            Value::String(text) => {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    segments.push(trimmed.to_string());
                }
            }
            _ => {}
        }
    }

    if segments.is_empty() {
        None
    } else {
        let mut combined = String::new();
        for (idx, segment) in segments.iter().enumerate() {
            if idx > 0 {
                combined.push('\n');
            }
            combined.push_str(segment);
        }
        Some(combined)
    }
}

fn parse_usage_value(value: &Value) -> Usage {
    let cache_read_tokens = value
        .get("prompt_cache_read_tokens")
        .or_else(|| value.get("cache_read_input_tokens"))
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);

    let cache_creation_tokens = value
        .get("prompt_cache_write_tokens")
        .or_else(|| value.get("cache_creation_input_tokens"))
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);

    Usage {
        prompt_tokens: value
            .get("prompt_tokens")
            .and_then(|pt| pt.as_u64())
            .unwrap_or(0) as u32,
        completion_tokens: value
            .get("completion_tokens")
            .and_then(|ct| ct.as_u64())
            .unwrap_or(0) as u32,
        total_tokens: value
            .get("total_tokens")
            .and_then(|tt| tt.as_u64())
            .unwrap_or(0) as u32,
        cached_prompt_tokens: cache_read_tokens,
        cache_creation_tokens,
        cache_read_tokens,
    }
}

fn map_finish_reason(reason: &str) -> FinishReason {
    match reason {
        "stop" | "completed" | "done" | "finished" => FinishReason::Stop,
        "length" => FinishReason::Length,
        "tool_calls" => FinishReason::ToolCalls,
        "content_filter" => FinishReason::ContentFilter,
        other => FinishReason::Error(other.to_string()),
    }
}

fn push_reasoning_value(reasoning: &mut ReasoningBuffer, value: &Value, deltas: &mut StreamDelta) {
    if let Some(reasoning_text) = extract_reasoning_trace(value) {
        if let Some(delta) = reasoning.push(&reasoning_text) {
            deltas.push_reasoning(&delta);
        }
    } else if let Some(text_value) = value.get("text").and_then(|v| v.as_str()) {
        if let Some(delta) = reasoning.push(text_value) {
            deltas.push_reasoning(&delta);
        }
    }
}

fn parse_chat_completion_chunk(
    payload: &Value,
    aggregated_content: &mut String,
    tool_call_builders: &mut Vec<ToolCallBuilder>,
    reasoning: &mut ReasoningBuffer,
    finish_reason: &mut FinishReason,
) -> StreamDelta {
    let mut deltas = StreamDelta::default();

    if let Some(choices) = payload.get("choices").and_then(|c| c.as_array()) {
        if let Some(choice) = choices.first() {
            if let Some(delta) = choice.get("delta") {
                if let Some(content_value) = delta.get("content") {
                    process_content_value(
                        content_value,
                        aggregated_content,
                        reasoning,
                        tool_call_builders,
                        &mut deltas,
                    );
                }

                if let Some(reasoning_value) = delta.get("reasoning") {
                    push_reasoning_value(reasoning, reasoning_value, &mut deltas);
                }

                if let Some(tool_calls_value) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                    update_tool_calls(tool_call_builders, tool_calls_value);
                }
            }

            if let Some(reasoning_value) = choice.get("reasoning") {
                push_reasoning_value(reasoning, reasoning_value, &mut deltas);
            }

            if let Some(reason) = choice.get("finish_reason").and_then(|v| v.as_str()) {
                *finish_reason = map_finish_reason(reason);
            }
        }
    }

    deltas
}

fn parse_response_chunk(
    payload: &Value,
    aggregated_content: &mut String,
    tool_call_builders: &mut Vec<ToolCallBuilder>,
    reasoning: &mut ReasoningBuffer,
    finish_reason: &mut FinishReason,
) -> StreamDelta {
    let mut deltas = StreamDelta::default();

    if let Some(delta_value) = payload.get("delta") {
        process_content_value(
            delta_value,
            aggregated_content,
            reasoning,
            tool_call_builders,
            &mut deltas,
        );
    }

    if let Some(event_type) = payload.get("type").and_then(|v| v.as_str()) {
        match event_type {
            "response.reasoning.delta" => {
                if let Some(delta_value) = payload.get("delta") {
                    push_reasoning_value(reasoning, delta_value, &mut deltas);
                }
            }
            "response.tool_call.delta" => {
                if let Some(delta_object) = payload.get("delta").and_then(|v| v.as_object()) {
                    apply_tool_call_delta_from_content(tool_call_builders, delta_object);
                }
            }
            "response.completed" | "response.done" | "response.finished" => {
                if let Some(response_obj) = payload.get("response") {
                    if aggregated_content.is_empty() {
                        process_content_value(
                            response_obj,
                            aggregated_content,
                            reasoning,
                            tool_call_builders,
                            &mut deltas,
                        );
                    }

                    if let Some(reason) = response_obj
                        .get("stop_reason")
                        .and_then(|value| value.as_str())
                        .or_else(|| response_obj.get("status").and_then(|value| value.as_str()))
                    {
                        *finish_reason = map_finish_reason(reason);
                    }
                }
            }
            _ => {}
        }
    }

    if let Some(response_obj) = payload.get("response") {
        if aggregated_content.is_empty() {
            if let Some(content_value) = response_obj
                .get("output_text")
                .or_else(|| response_obj.get("output"))
                .or_else(|| response_obj.get("content"))
            {
                process_content_value(
                    content_value,
                    aggregated_content,
                    reasoning,
                    tool_call_builders,
                    &mut deltas,
                );
            }
        }
    }

    if let Some(reasoning_value) = payload.get("reasoning") {
        push_reasoning_value(reasoning, reasoning_value, &mut deltas);
    }

    deltas
}

fn update_usage_from_value(source: &Value, usage: &mut Option<Usage>) {
    if let Some(usage_value) = source.get("usage") {
        *usage = Some(parse_usage_value(usage_value));
    }
}

fn extract_data_payload(event: &str) -> Option<String> {
    let mut data_lines: Vec<String> = Vec::new();

    for raw_line in event.lines() {
        let line = raw_line.trim_end_matches('\r');
        if line.is_empty() || line.starts_with(':') {
            continue;
        }

        if let Some(value) = line.strip_prefix("data:") {
            data_lines.push(value.trim_start().to_string());
        }
    }

    if data_lines.is_empty() {
        None
    } else {
        Some(data_lines.join("\n"))
    }
}

fn parse_stream_payload(
    payload: &Value,
    aggregated_content: &mut String,
    tool_call_builders: &mut Vec<ToolCallBuilder>,
    reasoning: &mut ReasoningBuffer,
    usage: &mut Option<Usage>,
    finish_reason: &mut FinishReason,
) -> Option<StreamDelta> {
    let mut emitted_delta = StreamDelta::default();

    let chat_delta = parse_chat_completion_chunk(
        payload,
        aggregated_content,
        tool_call_builders,
        reasoning,
        finish_reason,
    );
    emitted_delta.extend(chat_delta);

    let response_delta = parse_response_chunk(
        payload,
        aggregated_content,
        tool_call_builders,
        reasoning,
        finish_reason,
    );
    emitted_delta.extend(response_delta);

    update_usage_from_value(payload, usage);
    if let Some(response_obj) = payload.get("response") {
        update_usage_from_value(response_obj, usage);
        if let Some(reason) = response_obj
            .get("finish_reason")
            .and_then(|value| value.as_str())
        {
            *finish_reason = map_finish_reason(reason);
        }
    }

    if emitted_delta.is_empty() {
        None
    } else {
        Some(emitted_delta)
    }
}

fn finalize_stream_response(
    aggregated_content: String,
    tool_call_builders: Vec<ToolCallBuilder>,
    usage: Option<Usage>,
    finish_reason: FinishReason,
    reasoning: ReasoningBuffer,
) -> LLMResponse {
    let content = if aggregated_content.is_empty() {
        None
    } else {
        Some(aggregated_content)
    };

    let reasoning = reasoning.finalize();

    LLMResponse {
        content,
        tool_calls: finalize_tool_calls(tool_call_builders),
        usage,
        finish_reason,
        reasoning,
    }
}

pub struct OpenRouterProvider {
    api_key: String,
    http_client: HttpClient,
    base_url: String,
    model: String,
    prompt_cache_enabled: bool,
    prompt_cache_settings: OpenRouterPromptCacheSettings,
}

impl OpenRouterProvider {
    const TOOL_UNSUPPORTED_ERROR: &'static str = "No endpoints found that support tool use";

    pub fn new(api_key: String) -> Self {
        Self::with_model_internal(api_key, models::openrouter::DEFAULT_MODEL.to_string(), None)
    }

    pub fn with_model(api_key: String, model: String) -> Self {
        Self::with_model_internal(api_key, model, None)
    }

    pub fn from_config(
        api_key: Option<String>,
        model: Option<String>,
        base_url: Option<String>,
        prompt_cache: Option<PromptCachingConfig>,
    ) -> Self {
        let api_key_value = api_key.unwrap_or_default();
        let mut provider = if let Some(model_value) = model {
            Self::with_model_internal(api_key_value, model_value, prompt_cache)
        } else {
            Self::with_model_internal(
                api_key_value,
                models::openrouter::DEFAULT_MODEL.to_string(),
                prompt_cache,
            )
        };
        if let Some(base) = base_url {
            provider.base_url = base;
        }
        provider
    }

    fn with_model_internal(
        api_key: String,
        model: String,
        prompt_cache: Option<PromptCachingConfig>,
    ) -> Self {
        let (prompt_cache_enabled, prompt_cache_settings) =
            Self::extract_prompt_cache_settings(prompt_cache);

        Self {
            api_key,
            http_client: HttpClient::new(),
            base_url: urls::OPENROUTER_API_BASE.to_string(),
            model,
            prompt_cache_enabled,
            prompt_cache_settings,
        }
    }

    fn extract_prompt_cache_settings(
        prompt_cache: Option<PromptCachingConfig>,
    ) -> (bool, OpenRouterPromptCacheSettings) {
        if let Some(cfg) = prompt_cache {
            let provider_settings = cfg.providers.openrouter;
            let enabled = cfg.enabled && provider_settings.enabled;
            (enabled, provider_settings)
        } else {
            (false, OpenRouterPromptCacheSettings::default())
        }
    }

    fn default_request(&self, prompt: &str) -> LLMRequest {
        LLMRequest {
            messages: vec![Message::user(prompt.to_string())],
            system_prompt: None,
            tools: None,
            model: self.model.clone(),
            max_tokens: None,
            temperature: None,
            stream: false,
            tool_choice: None,
            parallel_tool_calls: None,
            parallel_tool_config: None,
            reasoning_effort: None,
        }
    }

    fn parse_client_prompt(&self, prompt: &str) -> LLMRequest {
        let trimmed = prompt.trim_start();
        if trimmed.starts_with('{') {
            if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
                if let Some(request) = self.parse_chat_request(&value) {
                    return request;
                }
            }
        }

        self.default_request(prompt)
    }

    fn is_gpt5_codex_model(model: &str) -> bool {
        model == models::openrouter::OPENAI_GPT_5_CODEX
    }

    fn resolve_model<'a>(&'a self, request: &'a LLMRequest) -> &'a str {
        if request.model.trim().is_empty() {
            self.model.as_str()
        } else {
            request.model.as_str()
        }
    }

    fn uses_responses_api_for(&self, request: &LLMRequest) -> bool {
        Self::is_gpt5_codex_model(self.resolve_model(request))
    }

    fn request_includes_tools(request: &LLMRequest) -> bool {
        request
            .tools
            .as_ref()
            .map(|tools| !tools.is_empty())
            .unwrap_or(false)
    }

    fn tool_free_request(original: &LLMRequest) -> LLMRequest {
        let mut sanitized = original.clone();
        sanitized.tools = None;
        sanitized.tool_choice = Some(ToolChoice::None);
        sanitized.parallel_tool_calls = None;
        sanitized
    }

    fn build_provider_payload(&self, request: &LLMRequest) -> Result<(Value, String), LLMError> {
        if self.uses_responses_api_for(request) {
            Ok((
                self.convert_to_openrouter_responses_format(request)?,
                format!("{}/responses", self.base_url),
            ))
        } else {
            Ok((
                self.convert_to_openrouter_format(request)?,
                format!("{}/chat/completions", self.base_url),
            ))
        }
    }

    async fn dispatch_request(&self, url: &str, payload: &Value) -> Result<Response, LLMError> {
        self.http_client
            .post(url)
            .bearer_auth(&self.api_key)
            .json(payload)
            .send()
            .await
            .map_err(|e| {
                let formatted_error =
                    error_display::format_llm_error("OpenRouter", &format!("Network error: {}", e));
                LLMError::Network(formatted_error)
            })
    }

    fn is_tool_unsupported_error(status: StatusCode, body: &str) -> bool {
        status == StatusCode::NOT_FOUND && body.contains(Self::TOOL_UNSUPPORTED_ERROR)
    }

    async fn send_with_tool_fallback(
        &self,
        request: &LLMRequest,
        stream_override: Option<bool>,
    ) -> Result<Response, LLMError> {
        let (mut payload, url) = self.build_provider_payload(request)?;
        if let Some(stream_flag) = stream_override {
            payload["stream"] = Value::Bool(stream_flag);
        }

        let response = self.dispatch_request(&url, &payload).await?;
        if response.status().is_success() {
            return Ok(response);
        }

        let status = response.status();
        let error_text = response.text().await.unwrap_or_default();

        if status.as_u16() == 429 || error_text.contains("quota") {
            return Err(LLMError::RateLimit);
        }

        if Self::request_includes_tools(request)
            && Self::is_tool_unsupported_error(status, &error_text)
        {
            let fallback_request = Self::tool_free_request(request);
            let (mut fallback_payload, fallback_url) =
                self.build_provider_payload(&fallback_request)?;
            if let Some(stream_flag) = stream_override {
                fallback_payload["stream"] = Value::Bool(stream_flag);
            }

            let fallback_response = self
                .dispatch_request(&fallback_url, &fallback_payload)
                .await?;
            if fallback_response.status().is_success() {
                return Ok(fallback_response);
            }

            let fallback_status = fallback_response.status();
            let fallback_text = fallback_response.text().await.unwrap_or_default();

            if fallback_status.as_u16() == 429 || fallback_text.contains("quota") {
                return Err(LLMError::RateLimit);
            }

            let combined_error = format!(
                "HTTP {}: {} | Tool fallback failed with HTTP {}: {}",
                status, error_text, fallback_status, fallback_text
            );
            let formatted_error = error_display::format_llm_error("OpenRouter", &combined_error);
            return Err(LLMError::Provider(formatted_error));
        }

        let formatted_error = error_display::format_llm_error(
            "OpenRouter",
            &format!("HTTP {}: {}", status, error_text),
        );
        Err(LLMError::Provider(formatted_error))
    }

    fn parse_chat_request(&self, value: &Value) -> Option<LLMRequest> {
        let messages_value = value.get("messages")?.as_array()?;
        let mut system_prompt = None;
        let mut messages = Vec::new();

        for entry in messages_value {
            let role = entry
                .get("role")
                .and_then(|r| r.as_str())
                .unwrap_or(crate::config::constants::message_roles::USER);
            let content = entry.get("content");
            let text_content = content.map(Self::extract_content_text).unwrap_or_default();

            match role {
                "system" => {
                    if system_prompt.is_none() && !text_content.is_empty() {
                        system_prompt = Some(text_content);
                    }
                }
                "assistant" => {
                    let tool_calls = entry
                        .get("tool_calls")
                        .and_then(|tc| tc.as_array())
                        .map(|calls| {
                            calls
                                .iter()
                                .filter_map(|call| {
                                    let id = call.get("id").and_then(|v| v.as_str())?;
                                    let function = call.get("function")?;
                                    let name = function.get("name").and_then(|v| v.as_str())?;
                                    let arguments = function.get("arguments");
                                    let serialized = arguments.map_or("{}".to_string(), |value| {
                                        if value.is_string() {
                                            value.as_str().unwrap_or("").to_string()
                                        } else {
                                            value.to_string()
                                        }
                                    });
                                    Some(ToolCall::function(
                                        id.to_string(),
                                        name.to_string(),
                                        serialized,
                                    ))
                                })
                                .collect::<Vec<_>>()
                        })
                        .filter(|calls| !calls.is_empty());

                    let message = if let Some(calls) = tool_calls {
                        Message {
                            role: MessageRole::Assistant,
                            content: text_content,
                            tool_calls: Some(calls),
                            tool_call_id: None,
                        }
                    } else {
                        Message::assistant(text_content)
                    };
                    messages.push(message);
                }
                "tool" => {
                    let tool_call_id = entry
                        .get("tool_call_id")
                        .and_then(|id| id.as_str())
                        .map(|s| s.to_string());
                    let content_value = entry
                        .get("content")
                        .map(|value| {
                            if text_content.is_empty() {
                                value.to_string()
                            } else {
                                text_content.clone()
                            }
                        })
                        .unwrap_or_else(|| text_content.clone());
                    messages.push(Message {
                        role: MessageRole::Tool,
                        content: content_value,
                        tool_calls: None,
                        tool_call_id,
                    });
                }
                _ => {
                    messages.push(Message::user(text_content));
                }
            }
        }

        if messages.is_empty() {
            return None;
        }

        let tools = value.get("tools").and_then(|tools_value| {
            let tools_array = tools_value.as_array()?;
            let converted: Vec<_> = tools_array
                .iter()
                .filter_map(|tool| {
                    let function = tool.get("function")?;
                    let name = function.get("name").and_then(|n| n.as_str())?;
                    let description = function
                        .get("description")
                        .and_then(|d| d.as_str())
                        .unwrap_or("")
                        .to_string();
                    let parameters = function
                        .get("parameters")
                        .cloned()
                        .unwrap_or_else(|| json!({}));
                    Some(ToolDefinition::function(
                        name.to_string(),
                        description,
                        parameters,
                    ))
                })
                .collect();

            if converted.is_empty() {
                None
            } else {
                Some(converted)
            }
        });

        let max_tokens = value
            .get("max_tokens")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32);
        let temperature = value
            .get("temperature")
            .and_then(|v| v.as_f64())
            .map(|v| v as f32);
        let stream = value
            .get("stream")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let tool_choice = value.get("tool_choice").and_then(Self::parse_tool_choice);
        let parallel_tool_calls = value.get("parallel_tool_calls").and_then(|v| v.as_bool());
        let reasoning_effort = value
            .get("reasoning_effort")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| {
                value
                    .get("reasoning")
                    .and_then(|r| r.get("effort"))
                    .and_then(|effort| effort.as_str())
                    .map(|s| s.to_string())
            });

        let model = value
            .get("model")
            .and_then(|m| m.as_str())
            .unwrap_or(&self.model)
            .to_string();

        Some(LLMRequest {
            messages,
            system_prompt,
            tools,
            model,
            max_tokens,
            temperature,
            stream,
            tool_choice,
            parallel_tool_calls,
            parallel_tool_config: None,
            reasoning_effort,
        })
    }

    fn extract_content_text(content: &Value) -> String {
        match content {
            Value::String(text) => text.to_string(),
            Value::Array(parts) => parts
                .iter()
                .filter_map(|part| {
                    if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                        Some(text.to_string())
                    } else if let Some(Value::String(text)) = part.get("content") {
                        Some(text.clone())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join(""),
            _ => String::new(),
        }
    }

    fn parse_tool_choice(choice: &Value) -> Option<ToolChoice> {
        match choice {
            Value::String(value) => match value.as_str() {
                "auto" => Some(ToolChoice::auto()),
                "none" => Some(ToolChoice::none()),
                "required" => Some(ToolChoice::any()),
                _ => None,
            },
            Value::Object(map) => {
                let choice_type = map.get("type").and_then(|t| t.as_str())?;
                match choice_type {
                    "function" => map
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|n| n.as_str())
                        .map(|name| ToolChoice::function(name.to_string())),
                    "auto" => Some(ToolChoice::auto()),
                    "none" => Some(ToolChoice::none()),
                    "any" | "required" => Some(ToolChoice::any()),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    fn build_standard_responses_input(&self, request: &LLMRequest) -> Result<Vec<Value>, LLMError> {
        let mut input = Vec::new();

        if let Some(system_prompt) = &request.system_prompt {
            if !system_prompt.trim().is_empty() {
                input.push(json!({
                    "role": "developer",
                    "content": [{
                        "type": "input_text",
                        "text": system_prompt.clone()
                    }]
                }));
            }
        }

        for msg in &request.messages {
            match msg.role {
                MessageRole::System => {
                    if !msg.content.trim().is_empty() {
                        input.push(json!({
                            "role": "developer",
                            "content": [{
                                "type": "input_text",
                                "text": msg.content.clone()
                            }]
                        }));
                    }
                }
                MessageRole::User => {
                    input.push(json!({
                        "role": "user",
                        "content": [{
                            "type": "input_text",
                            "text": msg.content.clone()
                        }]
                    }));
                }
                MessageRole::Assistant => {
                    let mut content_parts = Vec::new();
                    if !msg.content.is_empty() {
                        content_parts.push(json!({
                            "type": "output_text",
                            "text": msg.content.clone()
                        }));
                    }

                    if let Some(tool_calls) = &msg.tool_calls {
                        for call in tool_calls {
                            content_parts.push(json!({
                                "type": "tool_call",
                                "id": call.id.clone(),
                                "name": call.function.name.clone(),
                                "arguments": call.function.arguments.clone()
                            }));
                        }
                    }

                    if !content_parts.is_empty() {
                        input.push(json!({
                            "role": "assistant",
                            "content": content_parts
                        }));
                    }
                }
                MessageRole::Tool => {
                    let tool_call_id = msg.tool_call_id.clone().ok_or_else(|| {
                        let formatted_error = error_display::format_llm_error(
                            "OpenRouter",
                            "Tool messages must include tool_call_id for Responses API",
                        );
                        LLMError::InvalidRequest(formatted_error)
                    })?;

                    let mut tool_content = Vec::new();
                    if !msg.content.trim().is_empty() {
                        tool_content.push(json!({
                            "type": "output_text",
                            "text": msg.content.clone()
                        }));
                    }

                    let mut tool_result = json!({
                        "type": "tool_result",
                        "tool_call_id": tool_call_id
                    });

                    if !tool_content.is_empty() {
                        if let Value::Object(ref mut map) = tool_result {
                            map.insert("content".to_string(), json!(tool_content));
                        }
                    }

                    input.push(json!({
                        "role": "tool",
                        "content": [tool_result]
                    }));
                }
            }
        }

        Ok(input)
    }

    fn build_codex_responses_input(&self, request: &LLMRequest) -> Result<Vec<Value>, LLMError> {
        let mut additional_guidance = Vec::new();

        if let Some(system_prompt) = &request.system_prompt {
            let trimmed = system_prompt.trim();
            if !trimmed.is_empty() {
                additional_guidance.push(trimmed.to_string());
            }
        }

        let mut input = Vec::new();

        for msg in &request.messages {
            match msg.role {
                MessageRole::System => {
                    let trimmed = msg.content.trim();
                    if !trimmed.is_empty() {
                        additional_guidance.push(trimmed.to_string());
                    }
                }
                MessageRole::User => {
                    input.push(json!({
                        "role": "user",
                        "content": [{
                            "type": "input_text",
                            "text": msg.content.clone()
                        }]
                    }));
                }
                MessageRole::Assistant => {
                    let mut content_parts = Vec::new();
                    if !msg.content.is_empty() {
                        content_parts.push(json!({
                            "type": "output_text",
                            "text": msg.content.clone()
                        }));
                    }

                    if let Some(tool_calls) = &msg.tool_calls {
                        for call in tool_calls {
                            content_parts.push(json!({
                                "type": "tool_call",
                                "id": call.id.clone(),
                                "name": call.function.name.clone(),
                                "arguments": call.function.arguments.clone()
                            }));
                        }
                    }

                    if !content_parts.is_empty() {
                        input.push(json!({
                            "role": "assistant",
                            "content": content_parts
                        }));
                    }
                }
                MessageRole::Tool => {
                    let tool_call_id = msg.tool_call_id.clone().ok_or_else(|| {
                        let formatted_error = error_display::format_llm_error(
                            "OpenRouter",
                            "Tool messages must include tool_call_id for Responses API",
                        );
                        LLMError::InvalidRequest(formatted_error)
                    })?;

                    let mut tool_content = Vec::new();
                    if !msg.content.trim().is_empty() {
                        tool_content.push(json!({
                            "type": "output_text",
                            "text": msg.content.clone()
                        }));
                    }

                    let mut tool_result = json!({
                        "type": "tool_result",
                        "tool_call_id": tool_call_id
                    });

                    if !tool_content.is_empty() {
                        if let Value::Object(ref mut map) = tool_result {
                            map.insert("content".to_string(), json!(tool_content));
                        }
                    }

                    input.push(json!({
                        "role": "tool",
                        "content": [tool_result]
                    }));
                }
            }
        }

        let developer_prompt = gpt5_codex_developer_prompt(&additional_guidance);
        input.insert(
            0,
            json!({
                "role": "developer",
                "content": [{
                    "type": "input_text",
                    "text": developer_prompt
                }]
            }),
        );

        Ok(input)
    }

    fn convert_to_openrouter_responses_format(
        &self,
        request: &LLMRequest,
    ) -> Result<Value, LLMError> {
        let resolved_model = self.resolve_model(request);
        let input = if Self::is_gpt5_codex_model(resolved_model) {
            self.build_codex_responses_input(request)?
        } else {
            self.build_standard_responses_input(request)?
        };

        if input.is_empty() {
            let formatted_error = error_display::format_llm_error(
                "OpenRouter",
                "No messages provided for Responses API",
            );
            return Err(LLMError::InvalidRequest(formatted_error));
        }

        let mut provider_request = json!({
            "model": resolved_model,
            "input": input,
            "stream": request.stream
        });

        if let Some(max_tokens) = request.max_tokens {
            provider_request["max_output_tokens"] = json!(max_tokens);
        }

        if let Some(temperature) = request.temperature {
            provider_request["temperature"] = json!(temperature);
        }

        if let Some(tools) = &request.tools {
            if !tools.is_empty() {
                let tools_json: Vec<Value> = tools
                    .iter()
                    .map(|tool| {
                        json!({
                            "type": "function",
                            "function": {
                                "name": tool.function.name,
                                "description": tool.function.description,
                                "parameters": tool.function.parameters
                            }
                        })
                    })
                    .collect();
                provider_request["tools"] = Value::Array(tools_json);
            }
        }

        if let Some(tool_choice) = &request.tool_choice {
            provider_request["tool_choice"] = tool_choice.to_provider_format("openai");
        }

        if let Some(parallel) = request.parallel_tool_calls {
            provider_request["parallel_tool_calls"] = Value::Bool(parallel);
        }

        if let Some(effort) = request.reasoning_effort.as_deref() {
            if self.supports_reasoning_effort(resolved_model) {
                provider_request["reasoning"] = json!({ "effort": effort });
            }
        }

        if Self::is_gpt5_codex_model(resolved_model) {
            provider_request["reasoning"] = json!({ "effort": "medium" });
        }

        Ok(provider_request)
    }

    fn convert_to_openrouter_format(&self, request: &LLMRequest) -> Result<Value, LLMError> {
        let resolved_model = self.resolve_model(request);
        let mut messages = Vec::new();

        if let Some(system_prompt) = &request.system_prompt {
            messages.push(json!({
                "role": crate::config::constants::message_roles::SYSTEM,
                "content": system_prompt
            }));
        }

        for msg in &request.messages {
            let role = msg.role.as_openai_str();
            let mut message = json!({
                "role": role,
                "content": msg.content
            });

            if msg.role == MessageRole::Assistant {
                if let Some(tool_calls) = &msg.tool_calls {
                    if !tool_calls.is_empty() {
                        let tool_calls_json: Vec<Value> = tool_calls
                            .iter()
                            .map(|tc| {
                                json!({
                                    "id": tc.id,
                                    "type": "function",
                                    "function": {
                                        "name": tc.function.name,
                                        "arguments": tc.function.arguments
                                    }
                                })
                            })
                            .collect();
                        message["tool_calls"] = Value::Array(tool_calls_json);
                    }
                }
            }

            if msg.role == MessageRole::Tool {
                if let Some(tool_call_id) = &msg.tool_call_id {
                    message["tool_call_id"] = Value::String(tool_call_id.clone());
                }
            }

            messages.push(message);
        }

        if messages.is_empty() {
            let formatted_error =
                error_display::format_llm_error("OpenRouter", "No messages provided");
            return Err(LLMError::InvalidRequest(formatted_error));
        }

        let mut provider_request = json!({
            "model": resolved_model,
            "messages": messages,
            "stream": request.stream
        });

        if let Some(max_tokens) = request.max_tokens {
            provider_request["max_tokens"] = json!(max_tokens);
        }

        if let Some(temperature) = request.temperature {
            provider_request["temperature"] = json!(temperature);
        }

        if let Some(tools) = &request.tools {
            if !tools.is_empty() {
                let tools_json: Vec<Value> = tools
                    .iter()
                    .map(|tool| {
                        json!({
                            "type": "function",
                            "function": {
                                "name": tool.function.name,
                                "description": tool.function.description,
                                "parameters": tool.function.parameters
                            }
                        })
                    })
                    .collect();
                provider_request["tools"] = Value::Array(tools_json);
            }
        }

        if let Some(tool_choice) = &request.tool_choice {
            provider_request["tool_choice"] = tool_choice.to_provider_format("openai");
        }

        if let Some(parallel) = request.parallel_tool_calls {
            provider_request["parallel_tool_calls"] = Value::Bool(parallel);
        }

        if let Some(effort) = request.reasoning_effort.as_deref() {
            if self.supports_reasoning_effort(resolved_model) {
                provider_request["reasoning"] = json!({ "effort": effort });
            }
        }

        Ok(provider_request)
    }

    fn parse_openrouter_response(&self, response_json: Value) -> Result<LLMResponse, LLMError> {
        if let Some(choices) = response_json
            .get("choices")
            .and_then(|value| value.as_array())
        {
            if choices.is_empty() {
                let formatted_error =
                    error_display::format_llm_error("OpenRouter", "No choices in response");
                return Err(LLMError::Provider(formatted_error));
            }

            let choice = &choices[0];
            let message = choice.get("message").ok_or_else(|| {
                let formatted_error = error_display::format_llm_error(
                    "OpenRouter",
                    "Invalid response format: missing message",
                );
                LLMError::Provider(formatted_error)
            })?;

            let content = match message.get("content") {
                Some(Value::String(text)) => Some(text.to_string()),
                Some(Value::Array(parts)) => {
                    let text = parts
                        .iter()
                        .filter_map(|part| part.get("text").and_then(|t| t.as_str()))
                        .collect::<Vec<_>>()
                        .join("");
                    if text.is_empty() { None } else { Some(text) }
                }
                _ => None,
            };

            let tool_calls = message
                .get("tool_calls")
                .and_then(|tc| tc.as_array())
                .map(|calls| {
                    calls
                        .iter()
                        .filter_map(|call| {
                            let id = call.get("id").and_then(|v| v.as_str())?;
                            let function = call.get("function")?;
                            let name = function.get("name").and_then(|v| v.as_str())?;
                            let arguments = function.get("arguments");
                            let serialized = arguments.map_or("{}".to_string(), |value| {
                                if value.is_string() {
                                    value.as_str().unwrap_or("").to_string()
                                } else {
                                    value.to_string()
                                }
                            });
                            Some(ToolCall::function(
                                id.to_string(),
                                name.to_string(),
                                serialized,
                            ))
                        })
                        .collect::<Vec<_>>()
                })
                .filter(|calls| !calls.is_empty());

            let mut reasoning = message
                .get("reasoning")
                .and_then(extract_reasoning_trace)
                .or_else(|| choice.get("reasoning").and_then(extract_reasoning_trace));

            if reasoning.is_none() {
                reasoning = extract_reasoning_from_message_content(message);
            }

            let finish_reason = choice
                .get("finish_reason")
                .and_then(|fr| fr.as_str())
                .map(map_finish_reason)
                .unwrap_or(FinishReason::Stop);

            let usage = response_json.get("usage").map(parse_usage_value);

            return Ok(LLMResponse {
                content,
                tool_calls,
                usage,
                finish_reason,
                reasoning,
            });
        }

        self.parse_responses_api_response(&response_json)
    }

    fn parse_responses_api_response(&self, payload: &Value) -> Result<LLMResponse, LLMError> {
        let response_container = payload.get("response").unwrap_or(payload);

        let outputs = response_container
            .get("output")
            .or_else(|| response_container.get("outputs"))
            .and_then(|value| value.as_array())
            .ok_or_else(|| {
                let formatted_error = error_display::format_llm_error(
                    "OpenRouter",
                    "Invalid response format: missing output",
                );
                LLMError::Provider(formatted_error)
            })?;

        if outputs.is_empty() {
            let formatted_error =
                error_display::format_llm_error("OpenRouter", "No output in response");
            return Err(LLMError::Provider(formatted_error));
        }

        let message = outputs
            .iter()
            .find(|value| {
                value
                    .get("role")
                    .and_then(|role| role.as_str())
                    .map(|role| role == "assistant")
                    .unwrap_or(true)
            })
            .unwrap_or(&outputs[0]);

        let mut aggregated_content = String::new();
        let mut reasoning_buffer = ReasoningBuffer::default();
        let mut tool_call_builders: Vec<ToolCallBuilder> = Vec::new();
        let mut deltas = StreamDelta::default();

        if let Some(content_value) = message.get("content") {
            process_content_value(
                content_value,
                &mut aggregated_content,
                &mut reasoning_buffer,
                &mut tool_call_builders,
                &mut deltas,
            );
        } else {
            process_content_value(
                message,
                &mut aggregated_content,
                &mut reasoning_buffer,
                &mut tool_call_builders,
                &mut deltas,
            );
        }

        let mut tool_calls = finalize_tool_calls(tool_call_builders);
        if tool_calls.is_none() {
            tool_calls = extract_tool_calls_from_content(message);
        }

        let mut reasoning = reasoning_buffer.finalize();
        if reasoning.is_none() {
            reasoning = extract_reasoning_from_message_content(message)
                .or_else(|| message.get("reasoning").and_then(extract_reasoning_trace))
                .or_else(|| payload.get("reasoning").and_then(extract_reasoning_trace));
        }

        let content = if aggregated_content.is_empty() {
            message
                .get("output_text")
                .and_then(|value| value.as_str())
                .map(|value| value.to_string())
        } else {
            Some(aggregated_content)
        };

        let mut usage = payload.get("usage").map(parse_usage_value);
        if usage.is_none() {
            usage = response_container.get("usage").map(parse_usage_value);
        }

        let finish_reason = payload
            .get("stop_reason")
            .or_else(|| payload.get("finish_reason"))
            .or_else(|| payload.get("status"))
            .or_else(|| response_container.get("stop_reason"))
            .or_else(|| response_container.get("finish_reason"))
            .or_else(|| message.get("stop_reason"))
            .or_else(|| message.get("finish_reason"))
            .and_then(|value| value.as_str())
            .map(map_finish_reason)
            .unwrap_or(FinishReason::Stop);

        Ok(LLMResponse {
            content,
            tool_calls,
            usage,
            finish_reason,
            reasoning,
        })
    }
}

#[async_trait]
impl LLMProvider for OpenRouterProvider {
    fn name(&self) -> &str {
        "openrouter"
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    fn supports_reasoning(&self, model: &str) -> bool {
        let requested = if model.trim().is_empty() {
            self.model.as_str()
        } else {
            model
        };

        models::openrouter::REASONING_MODELS
            .iter()
            .any(|candidate| *candidate == requested)
    }

    fn supports_reasoning_effort(&self, model: &str) -> bool {
        let requested = if model.trim().is_empty() {
            self.model.as_str()
        } else {
            model
        };
        models::openrouter::REASONING_MODELS
            .iter()
            .any(|candidate| *candidate == requested)
    }

    fn supports_tools(&self, model: &str) -> bool {
        let requested = if model.trim().is_empty() {
            self.model.as_str()
        } else {
            model
        };

        !models::openrouter::TOOL_UNAVAILABLE_MODELS
            .iter()
            .any(|candidate| *candidate == requested)
    }

    async fn stream(&self, request: LLMRequest) -> Result<LLMStream, LLMError> {
        let response = self.send_with_tool_fallback(&request, Some(true)).await?;

        fn find_sse_boundary(buffer: &str) -> Option<(usize, usize)> {
            let newline_boundary = buffer.find("\n\n").map(|idx| (idx, 2));
            let carriage_boundary = buffer.find("\r\n\r\n").map(|idx| (idx, 4));

            match (newline_boundary, carriage_boundary) {
                (Some((n_idx, n_len)), Some((c_idx, c_len))) => {
                    if n_idx <= c_idx {
                        Some((n_idx, n_len))
                    } else {
                        Some((c_idx, c_len))
                    }
                }
                (Some(boundary), None) => Some(boundary),
                (None, Some(boundary)) => Some(boundary),
                (None, None) => None,
            }
        }

        let stream = try_stream! {
            let mut body_stream = response.bytes_stream();
            let mut buffer = String::new();
            let mut aggregated_content = String::new();
            let mut tool_call_builders: Vec<ToolCallBuilder> = Vec::new();
            let mut reasoning = ReasoningBuffer::default();
            let mut usage: Option<Usage> = None;
            let mut finish_reason = FinishReason::Stop;
            let mut done = false;

            while let Some(chunk_result) = body_stream.next().await {
                let chunk = chunk_result.map_err(|err| {
                    let formatted_error = error_display::format_llm_error(
                        "OpenRouter",
                        &format!("Streaming error: {}", err),
                    );
                    LLMError::Network(formatted_error)
                })?;

                buffer.push_str(&String::from_utf8_lossy(&chunk));

                while let Some((split_idx, delimiter_len)) = find_sse_boundary(&buffer) {
                    let event = buffer[..split_idx].to_string();
                    buffer.drain(..split_idx + delimiter_len);

                    if let Some(data_payload) = extract_data_payload(&event) {
                        let trimmed_payload = data_payload.trim();
                        if trimmed_payload == "[DONE]" {
                            done = true;
                            break;
                        }

                        if !trimmed_payload.is_empty() {
                            let payload: Value = serde_json::from_str(trimmed_payload).map_err(|err| {
                                let formatted_error = error_display::format_llm_error(
                                    "OpenRouter",
                                    &format!("Failed to parse stream payload: {}", err),
                                );
                                LLMError::Provider(formatted_error)
                            })?;

                            if let Some(delta) = parse_stream_payload(
                                &payload,
                                &mut aggregated_content,
                                &mut tool_call_builders,
                                &mut reasoning,
                                &mut usage,
                                &mut finish_reason,
                            ) {
                                for fragment in delta.into_fragments() {
                                    match fragment {
                                        StreamFragment::Content(text) if !text.is_empty() => {
                                            yield LLMStreamEvent::Token { delta: text };
                                        }
                                        StreamFragment::Reasoning(text) if !text.is_empty() => {
                                            yield LLMStreamEvent::Reasoning { delta: text };
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }
                    }
                }

                if done {
                    break;
                }
            }

            if !done && !buffer.trim().is_empty() {
                if let Some(data_payload) = extract_data_payload(&buffer) {
                    let trimmed_payload = data_payload.trim();
                    if trimmed_payload != "[DONE]" && !trimmed_payload.is_empty() {
                        let payload: Value = serde_json::from_str(trimmed_payload).map_err(|err| {
                            let formatted_error = error_display::format_llm_error(
                                "OpenRouter",
                                &format!("Failed to parse stream payload: {}", err),
                            );
                            LLMError::Provider(formatted_error)
                        })?;

                        if let Some(delta) = parse_stream_payload(
                            &payload,
                            &mut aggregated_content,
                            &mut tool_call_builders,
                            &mut reasoning,
                            &mut usage,
                            &mut finish_reason,
                        ) {
                            for fragment in delta.into_fragments() {
                                match fragment {
                                    StreamFragment::Content(text) if !text.is_empty() => {
                                        yield LLMStreamEvent::Token { delta: text };
                                    }
                                    StreamFragment::Reasoning(text) if !text.is_empty() => {
                                        yield LLMStreamEvent::Reasoning { delta: text };
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                }
            }

            let response = finalize_stream_response(
                aggregated_content,
                tool_call_builders,
                usage,
                finish_reason,
                reasoning,
            );

            yield LLMStreamEvent::Completed { response };
        };

        Ok(Box::pin(stream))
    }

    async fn generate(&self, request: LLMRequest) -> Result<LLMResponse, LLMError> {
        if self.prompt_cache_enabled && self.prompt_cache_settings.propagate_provider_capabilities {
            // When enabled, vtcode forwards provider-specific cache_control markers directly
            // through the OpenRouter payload without further transformation.
        }

        if self.prompt_cache_enabled && self.prompt_cache_settings.report_savings {
            // Cache savings are surfaced via usage metrics parsed later in the response cycle.
        }

        let response = self.send_with_tool_fallback(&request, None).await?;

        let openrouter_response: Value = response.json().await.map_err(|e| {
            let formatted_error = error_display::format_llm_error(
                "OpenRouter",
                &format!("Failed to parse response: {}", e),
            );
            LLMError::Provider(formatted_error)
        })?;

        self.parse_openrouter_response(openrouter_response)
    }

    fn supported_models(&self) -> Vec<String> {
        models::openrouter::SUPPORTED_MODELS
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    fn validate_request(&self, request: &LLMRequest) -> Result<(), LLMError> {
        if request.messages.is_empty() {
            let formatted_error =
                error_display::format_llm_error("OpenRouter", "Messages cannot be empty");
            return Err(LLMError::InvalidRequest(formatted_error));
        }

        for message in &request.messages {
            if let Err(err) = message.validate_for_provider("openai") {
                let formatted = error_display::format_llm_error("OpenRouter", &err);
                return Err(LLMError::InvalidRequest(formatted));
            }
        }

        if request.model.trim().is_empty() {
            let formatted_error =
                error_display::format_llm_error("OpenRouter", "Model must be provided");
            return Err(LLMError::InvalidRequest(formatted_error));
        }

        Ok(())
    }
}

#[async_trait]
impl LLMClient for OpenRouterProvider {
    async fn generate(&mut self, prompt: &str) -> Result<llm_types::LLMResponse, LLMError> {
        let request = self.parse_client_prompt(prompt);
        let request_model = request.model.clone();
        let response = LLMProvider::generate(self, request).await?;

        Ok(llm_types::LLMResponse {
            content: response.content.unwrap_or_default(),
            model: request_model,
            usage: response.usage.map(|u| llm_types::Usage {
                prompt_tokens: u.prompt_tokens as usize,
                completion_tokens: u.completion_tokens as usize,
                total_tokens: u.total_tokens as usize,
                cached_prompt_tokens: u.cached_prompt_tokens.map(|v| v as usize),
                cache_creation_tokens: u.cache_creation_tokens.map(|v| v as usize),
                cache_read_tokens: u.cache_read_tokens.map(|v| v as usize),
            }),
            reasoning: response.reasoning,
        })
    }

    fn backend_kind(&self) -> llm_types::BackendKind {
        llm_types::BackendKind::OpenRouter
    }

    fn model_id(&self) -> &str {
        &self.model
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_parse_stream_payload_chat_chunk() {
        let payload = json!({
            "choices": [{
                "delta": {
                    "content": [
                        {"type": "output_text", "text": "Hello"}
                    ]
                }
            }]
        });

        let mut aggregated = String::new();
        let mut builders = Vec::new();
        let mut reasoning = ReasoningBuffer::default();
        let mut usage = None;
        let mut finish_reason = FinishReason::Stop;

        let delta = parse_stream_payload(
            &payload,
            &mut aggregated,
            &mut builders,
            &mut reasoning,
            &mut usage,
            &mut finish_reason,
        );

        let fragments = delta.expect("delta should exist").into_fragments();
        assert_eq!(
            fragments,
            vec![StreamFragment::Content("Hello".to_string())]
        );
        assert_eq!(aggregated, "Hello");
        assert!(builders.is_empty());
        assert!(usage.is_none());
        assert!(reasoning.finalize().is_none());
    }

    #[test]
    fn test_parse_stream_payload_response_delta() {
        let payload = json!({
            "type": "response.delta",
            "delta": {
                "type": "output_text_delta",
                "text": "Stream"
            }
        });

        let mut aggregated = String::new();
        let mut builders = Vec::new();
        let mut reasoning = ReasoningBuffer::default();
        let mut usage = None;
        let mut finish_reason = FinishReason::Stop;

        let delta = parse_stream_payload(
            &payload,
            &mut aggregated,
            &mut builders,
            &mut reasoning,
            &mut usage,
            &mut finish_reason,
        );

        let fragments = delta.expect("delta should exist").into_fragments();
        assert_eq!(
            fragments,
            vec![StreamFragment::Content("Stream".to_string())]
        );
        assert_eq!(aggregated, "Stream");
    }

    #[test]
    fn test_extract_data_payload_joins_multiline_events() {
        let event = ": keep-alive\n".to_string() + "data: {\"a\":1}\n" + "data: {\"b\":2}\n";
        let payload = extract_data_payload(&event);
        assert_eq!(payload.as_deref(), Some("{\"a\":1}\n{\"b\":2}"));
    }

    #[test]
    fn parse_usage_value_includes_cache_metrics() {
        let value = json!({
            "prompt_tokens": 120,
            "completion_tokens": 80,
            "total_tokens": 200,
            "prompt_cache_read_tokens": 90,
            "prompt_cache_write_tokens": 15
        });

        let usage = parse_usage_value(&value);
        assert_eq!(usage.prompt_tokens, 120);
        assert_eq!(usage.completion_tokens, 80);
        assert_eq!(usage.total_tokens, 200);
        assert_eq!(usage.cached_prompt_tokens, Some(90));
        assert_eq!(usage.cache_read_tokens, Some(90));
        assert_eq!(usage.cache_creation_tokens, Some(15));
    }
}
