use std::collections::HashSet;

use vtcode_core::config::loader::VTCodeConfig;
use vtcode_core::config::types::AgentConfig as CoreAgentConfig;
use vtcode_core::llm::{factory::create_provider_with_config, provider as uni};

const MIN_PROMPT_LENGTH_FOR_REFINEMENT: usize = 20;
const MIN_PROMPT_WORDS_FOR_REFINEMENT: usize = 4;
const SHORT_PROMPT_WORD_THRESHOLD: usize = 6;
const MAX_REFINED_WORD_MULTIPLIER: usize = 3;
const MIN_KEYWORD_LENGTH: usize = 3;
const MIN_KEYWORD_OVERLAP_RATIO: f32 = 0.5;

pub(crate) async fn refine_user_prompt_if_enabled(
    raw: &str,
    cfg: &CoreAgentConfig,
    vt_cfg: Option<&VTCodeConfig>,
) -> String {
    if std::env::var("VTCODE_PROMPT_REFINER_STUB").is_ok() {
        return format!("[REFINED] {}", raw);
    }
    let Some(vtc) = vt_cfg else {
        return raw.to_string();
    };
    if !vtc.agent.refine_prompts_enabled {
        return raw.to_string();
    }

    if !should_attempt_refinement(raw) {
        return raw.to_string();
    }

    let provider_name = if cfg.provider.trim().is_empty() {
        "gemini".to_string()
    } else {
        cfg.provider.to_lowercase()
    };

    let refiner_model = if !vtc.agent.refine_prompts_model.is_empty() {
        vtc.agent.refine_prompts_model.clone()
    } else {
        match provider_name.as_str() {
            "openai" => vtcode_core::config::constants::models::openai::GPT_5_MINI.to_string(),
            _ => cfg.model.clone(),
        }
    };

    let Ok(refiner) = create_provider_with_config(
        &provider_name,
        Some(cfg.api_key.clone()),
        None,
        Some(refiner_model.clone()),
    ) else {
        return raw.to_string();
    };

    let req = uni::LLMRequest {
        messages: vec![uni::Message::user(raw.to_string())],
        system_prompt: None,
        tools: None,
        model: refiner_model,
        max_tokens: Some(800),
        temperature: Some(0.3),
        stream: false,
        tool_choice: Some(uni::ToolChoice::none()),
        parallel_tool_calls: None,
        parallel_tool_config: None,
        reasoning_effort: None,
    };

    match refiner
        .generate(req)
        .await
        .map(|response| response.content.unwrap_or_default())
    {
        Ok(text) if should_accept_refinement(raw, &text) => text,
        _ => raw.to_string(),
    }
}

fn should_attempt_refinement(raw: &str) -> bool {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return false;
    }

    let char_len = trimmed.chars().count();
    let word_count = trimmed.split_whitespace().count();

    char_len >= MIN_PROMPT_LENGTH_FOR_REFINEMENT && word_count >= MIN_PROMPT_WORDS_FOR_REFINEMENT
}

fn should_accept_refinement(raw: &str, refined: &str) -> bool {
    let trimmed = refined.trim();
    if trimmed.is_empty() {
        return false;
    }

    if trimmed.eq_ignore_ascii_case(raw.trim()) {
        return true;
    }

    let raw_words: Vec<&str> = raw.split_whitespace().collect();
    if raw_words.len() < MIN_PROMPT_WORDS_FOR_REFINEMENT {
        return false;
    }

    let refined_words: Vec<&str> = trimmed.split_whitespace().collect();
    if raw_words.len() <= SHORT_PROMPT_WORD_THRESHOLD
        && refined_words.len() > raw_words.len() * MAX_REFINED_WORD_MULTIPLIER
    {
        return false;
    }

    let refined_lower = trimmed.to_lowercase();
    let suspicious_prefixes = ["hello", "hi", "hey", "greetings", "i'm", "i am"];
    if suspicious_prefixes
        .iter()
        .any(|prefix| refined_lower.starts_with(prefix))
    {
        return false;
    }
    let suspicious_phrases = ["how can i help you", "i'm here to", "let me know if"];
    if suspicious_phrases
        .iter()
        .any(|phrase| refined_lower.contains(phrase))
    {
        return false;
    }

    let raw_keywords = keyword_set(raw);
    if raw_keywords.is_empty() {
        return true;
    }
    let refined_keywords = keyword_set(trimmed);
    let overlap = raw_keywords.intersection(&refined_keywords).count() as f32;
    let ratio = overlap / raw_keywords.len() as f32;
    ratio >= MIN_KEYWORD_OVERLAP_RATIO
}

fn keyword_set(text: &str) -> HashSet<String> {
    text.split_whitespace()
        .map(|token| token.trim_matches(|ch: char| !ch.is_alphanumeric()))
        .filter(|token| token.len() >= MIN_KEYWORD_LENGTH)
        .map(|token| token.to_ascii_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_prompt_refinement_applies_to_gemini_when_flag_disabled() {
        unsafe {
            std::env::set_var("VTCODE_PROMPT_REFINER_STUB", "1");
        }

        let cfg = CoreAgentConfig {
            model: vtcode_core::config::constants::models::google::GEMINI_2_5_FLASH_PREVIEW
                .to_string(),
            api_key: "test".to_string(),
            provider: "gemini".to_string(),
            workspace: std::env::current_dir().unwrap(),
            verbose: false,
            theme: vtcode_core::ui::theme::DEFAULT_THEME_ID.to_string(),
        };

        let mut vt = VTCodeConfig::default();
        vt.agent.refine_prompts_enabled = true;

        let raw = "make me a list of files";
        let out = refine_user_prompt_if_enabled(raw, &cfg, Some(&vt)).await;

        assert!(out.starts_with("[REFINED] "));

        unsafe {
            std::env::remove_var("VTCODE_PROMPT_REFINER_STUB");
        }
    }

    #[test]
    fn test_should_attempt_refinement_skips_short_inputs() {
        assert!(!should_attempt_refinement("hi"));
        assert!(!should_attempt_refinement("add docs"));
        assert!(should_attempt_refinement(
            "summarize the latest commit changes"
        ));
    }

    #[test]
    fn test_should_accept_refinement_rejects_role_play() {
        let raw = "hello";
        let refined = "Hello! How can I help you today?";
        assert!(!should_accept_refinement(raw, refined));

        let technical_raw = "describe vtcode streaming parser";
        let technical_refined =
            "Provide a detailed description of the vtcode streaming parser implementation.";
        assert!(should_accept_refinement(technical_raw, technical_refined));
    }
}
