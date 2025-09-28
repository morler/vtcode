use anyhow::Result;
use tempfile::TempDir;
use tokio::time::{Duration, sleep};
use vtcode_core::{
    Agent,
    config::ReasoningEffortLevel,
    config::constants::models::google::GEMINI_2_5_FLASH_PREVIEW,
    config::core::PromptCachingConfig,
    config::types::{AgentConfig, ModelSelectionSource, UiSurfacePreference},
    handle_stats_command,
    ui::theme::DEFAULT_THEME_ID,
};

#[tokio::test]
#[ignore]
async fn test_handle_stats_command_returns_agent_metrics() -> Result<()> {
    let temp_dir = TempDir::new()?;
    let config = AgentConfig {
        model: GEMINI_2_5_FLASH_PREVIEW.to_string(),
        api_key: "test_key".to_string(),
        provider: "gemini".to_string(),
        workspace: temp_dir.path().to_path_buf(),
        verbose: false,
        theme: DEFAULT_THEME_ID.to_string(),
        reasoning_effort: ReasoningEffortLevel::default(),
        ui_surface: UiSurfacePreference::default(),
        prompt_cache: PromptCachingConfig::default(),
        model_source: ModelSelectionSource::WorkspaceConfig,
    };
    let mut agent = Agent::new(config)?;
    agent.update_session_stats(5, 3, 1);
    sleep(Duration::from_millis(10)).await;
    let metrics = handle_stats_command(&agent, false, "json".to_string()).await?;
    assert_eq!(metrics.total_api_calls, 5);
    assert_eq!(metrics.tool_execution_count, 3);
    assert_eq!(metrics.error_count, 1);
    assert!(metrics.session_duration_seconds > 0);
    Ok(())
}
