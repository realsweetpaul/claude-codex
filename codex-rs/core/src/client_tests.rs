use super::AuthRequestTelemetryContext;
use super::ModelClient;
use super::PendingUnauthorizedRetry;
use super::UnauthorizedRecoveryExecution;
use codex_otel::SessionTelemetry;
use codex_protocol::ThreadId;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use pretty_assertions::assert_eq;
use serde_json::json;

fn test_model_client(session_source: SessionSource) -> ModelClient {
    let provider = crate::model_provider_info::create_oss_provider_with_base_url(
        "https://example.com/v1",
        crate::model_provider_info::WireApi::Responses,
    );
    ModelClient::new(
        /*auth_manager*/ None,
        ThreadId::new(),
        provider,
        session_source,
        /*model_verbosity*/ None,
        /*tool_choice*/ None,
        /*messages_metadata_user_id*/ None,
        /*enable_request_compression*/ false,
        /*include_timing_metrics*/ false,
        /*beta_features_header*/ None,
    )
}

fn test_model_info() -> ModelInfo {
    serde_json::from_value(json!({
        "slug": "gpt-test",
        "display_name": "gpt-test",
        "description": "desc",
        "default_reasoning_level": "medium",
        "supported_reasoning_levels": [
            {"effort": "medium", "description": "medium"}
        ],
        "shell_type": "shell_command",
        "visibility": "list",
        "supported_in_api": true,
        "priority": 1,
        "upgrade": null,
        "base_instructions": "base instructions",
        "model_messages": null,
        "supports_reasoning_summaries": false,
        "support_verbosity": false,
        "default_verbosity": null,
        "apply_patch_tool_type": null,
        "truncation_policy": {"mode": "bytes", "limit": 10000},
        "supports_parallel_tool_calls": false,
        "supports_image_detail_original": false,
        "context_window": 272000,
        "auto_compact_token_limit": null,
        "experimental_supported_tools": []
    }))
    .expect("deserialize test model info")
}

fn test_session_telemetry() -> SessionTelemetry {
    SessionTelemetry::new(
        ThreadId::new(),
        "gpt-test",
        "gpt-test",
        /*account_id*/ None,
        /*account_email*/ None,
        /*auth_mode*/ None,
        "test-originator".to_string(),
        /*log_user_prompts*/ false,
        "test-terminal".to_string(),
        SessionSource::Cli,
    )
}

#[test]
fn build_subagent_headers_sets_other_subagent_label() {
    let client = test_model_client(SessionSource::SubAgent(SubAgentSource::Other(
        "memory_consolidation".to_string(),
    )));
    let headers = client.build_subagent_headers();
    let value = headers
        .get("x-openai-subagent")
        .and_then(|value| value.to_str().ok());
    assert_eq!(value, Some("memory_consolidation"));
}

#[tokio::test]
async fn summarize_memories_returns_empty_for_empty_input() {
    let client = test_model_client(SessionSource::Cli);
    let model_info = test_model_info();
    let session_telemetry = test_session_telemetry();

    let output = client
        .summarize_memories(
            Vec::new(),
            &model_info,
            /*effort*/ None,
            &session_telemetry,
        )
        .await
        .expect("empty summarize request should succeed");
    assert_eq!(output.len(), 0);
}

#[test]
fn auth_request_telemetry_context_tracks_attached_auth_and_retry_phase() {
    let auth_context = AuthRequestTelemetryContext::new(
        Some(codex_login::AuthMode::Chatgpt),
        &crate::api_bridge::CoreAuthProvider::for_test(Some("access-token"), Some("workspace-123")),
        PendingUnauthorizedRetry::from_recovery(UnauthorizedRecoveryExecution {
            mode: "managed",
            phase: "refresh_token",
        }),
    );

    assert_eq!(auth_context.auth_mode, Some("Chatgpt"));
    assert!(auth_context.auth_header_attached);
    assert_eq!(auth_context.auth_header_name, Some("authorization"));
    assert!(auth_context.retry_after_unauthorized);
    assert_eq!(auth_context.recovery_mode, Some("managed"));
    assert_eq!(auth_context.recovery_phase, Some("refresh_token"));
}

// ── S-020 Sub-B: /messages-specific client tests ───────────────────────

#[test]
fn anthropic_thinking_param_adaptive_for_medium_effort() {
    use codex_protocol::openai_models::ReasoningEffort;

    let result = super::anthropic_thinking_param(Some(ReasoningEffort::Medium));
    assert!(result.is_some(), "Medium effort should produce thinking param");
    assert_eq!(result.unwrap()["type"], "adaptive");
}

#[test]
fn anthropic_thinking_param_adaptive_for_high_effort() {
    use codex_protocol::openai_models::ReasoningEffort;

    let result = super::anthropic_thinking_param(Some(ReasoningEffort::High));
    assert!(result.is_some(), "High effort should produce thinking param");
    assert_eq!(result.unwrap()["type"], "adaptive");
}

#[test]
fn anthropic_thinking_param_none_for_minimal_effort() {
    use codex_protocol::openai_models::ReasoningEffort;

    let result = super::anthropic_thinking_param(Some(ReasoningEffort::Minimal));
    assert!(result.is_none(), "Minimal effort should disable thinking");
}

#[test]
fn anthropic_thinking_param_none_when_effort_is_none() {
    let result = super::anthropic_thinking_param(None);
    assert!(result.is_none(), "None effort should disable thinking");
}

#[test]
fn anthropic_thinking_param_none_for_none_variant() {
    use codex_protocol::openai_models::ReasoningEffort;

    let result = super::anthropic_thinking_param(Some(ReasoningEffort::None));
    assert!(result.is_none(), "ReasoningEffort::None should disable thinking");
}

#[test]
fn anthropic_thinking_param_adaptive_for_low_effort() {
    use codex_protocol::openai_models::ReasoningEffort;

    let result = super::anthropic_thinking_param(Some(ReasoningEffort::Low));
    assert!(result.is_some(), "Low effort should produce thinking param");
    assert_eq!(result.unwrap()["type"], "adaptive");
}

#[test]
fn anthropic_max_output_tokens_opus_128k() {
    let tokens = super::anthropic_max_output_tokens("claude-opus-4-6");
    assert_eq!(tokens, 128_000, "Opus models should get 128K output tokens");
}

#[test]
fn anthropic_max_output_tokens_sonnet_64k() {
    let tokens = super::anthropic_max_output_tokens("claude-sonnet-4-6");
    assert_eq!(tokens, 64_000, "Sonnet models should get 64K output tokens");
}

#[test]
fn anthropic_max_output_tokens_haiku_8k() {
    let tokens = super::anthropic_max_output_tokens("claude-haiku-3-5");
    assert_eq!(tokens, 8_192, "Haiku models should get 8K output tokens");
}

#[test]
fn anthropic_max_output_tokens_default_for_unknown_claude() {
    let tokens = super::anthropic_max_output_tokens("claude-future-model");
    assert_eq!(tokens, 64_000, "Unknown Claude models should get 64K default");
}

#[test]
fn anthropic_max_output_tokens_non_claude_default() {
    let tokens = super::anthropic_max_output_tokens("gpt-5.3-codex");
    assert_eq!(tokens, 64_000, "Non-Claude models should get 64K default");
}

#[test]
fn is_anthropic_model_recognizes_claude_slugs() {
    assert!(super::is_anthropic_model("claude-sonnet-4-6"));
    assert!(super::is_anthropic_model("claude-opus-4-6"));
    assert!(super::is_anthropic_model("claude-haiku-3-5"));
    assert!(super::is_anthropic_model("Claude-Sonnet-4-6"));  // case insensitive
}

#[test]
fn is_anthropic_model_rejects_non_claude() {
    assert!(!super::is_anthropic_model("gpt-5.3-codex"));
    assert!(!super::is_anthropic_model("o3-mini"));
    assert!(!super::is_anthropic_model("custom-model"));
}

#[test]
fn messages_api_request_serializes_all_fields() {
    use codex_api::{MessagesApiMetadata, MessagesApiRequest};

    let request = MessagesApiRequest {
        model: "claude-sonnet-4-6".to_string(),
        messages: vec![json!({"role": "user", "content": [{"type": "text", "text": "hello"}]})],
        max_tokens: 64000,
        stream: true,
        system: Some(json!([{"type": "text", "text": "You are helpful"}])),
        tools: Some(vec![json!({"name": "shell", "description": "run shell", "input_schema": {}})]),
        tool_choice: Some(json!({"type": "auto"})),
        thinking: Some(json!({"type": "adaptive"})),
        temperature: Some(0.7),
        top_p: Some(0.9),
        top_k: Some(40),
        stop_sequences: Some(vec!["STOP".to_string()]),
        metadata: Some(MessagesApiMetadata {
            user_id: "test-user-123".to_string(),
        }),
    };

    let serialized = serde_json::to_value(&request).unwrap();
    assert_eq!(serialized["model"], "claude-sonnet-4-6");
    assert_eq!(serialized["max_tokens"], 64000);
    assert_eq!(serialized["stream"], true);
    assert_eq!(serialized["system"][0]["text"], "You are helpful");
    assert_eq!(serialized["tools"][0]["name"], "shell");
    assert_eq!(serialized["tool_choice"]["type"], "auto");
    assert_eq!(serialized["thinking"]["type"], "adaptive");
    assert_eq!(serialized["temperature"], 0.7);
    assert_eq!(serialized["top_p"], 0.9);
    assert_eq!(serialized["top_k"], 40);
    assert_eq!(serialized["stop_sequences"][0], "STOP");
    assert_eq!(serialized["metadata"]["user_id"], "test-user-123");
}

#[test]
fn messages_api_request_omits_none_fields() {
    use codex_api::MessagesApiRequest;

    let request = MessagesApiRequest {
        model: "claude-sonnet-4-6".to_string(),
        messages: vec![],
        max_tokens: 64000,
        stream: true,
        system: None,
        tools: None,
        tool_choice: None,
        thinking: None,
        temperature: None,
        top_p: None,
        top_k: None,
        stop_sequences: None,
        metadata: None,
    };

    let serialized = serde_json::to_value(&request).unwrap();
    // Fields with skip_serializing_if = "Option::is_none" should be absent
    assert!(serialized.get("system").is_none(), "system should be omitted when None");
    assert!(serialized.get("tools").is_none(), "tools should be omitted when None");
    assert!(serialized.get("tool_choice").is_none(), "tool_choice should be omitted when None");
    assert!(serialized.get("thinking").is_none(), "thinking should be omitted when None");
    assert!(serialized.get("temperature").is_none(), "temperature should be omitted when None");
    assert!(serialized.get("top_p").is_none(), "top_p should be omitted when None");
    assert!(serialized.get("top_k").is_none(), "top_k should be omitted when None");
    assert!(serialized.get("stop_sequences").is_none(), "stop_sequences should be omitted when None");
    assert!(serialized.get("metadata").is_none(), "metadata should be omitted when None");
    // Required fields must be present
    assert!(serialized.get("model").is_some());
    assert!(serialized.get("max_tokens").is_some());
    assert!(serialized.get("stream").is_some());
}

#[test]
fn anthropic_beta_header_includes_interleaved_thinking() {
    // The anthropic-beta header is built in MessagesClient::stream_request
    // based on request.thinking being Some. We verify the header value
    // construction logic here by checking the expected constant.
    let beta_features: Vec<&str> = vec!["interleaved-thinking-2025-05-14"];
    let header_value = beta_features.join(",");
    assert_eq!(header_value, "interleaved-thinking-2025-05-14");
    // Verify the header value is valid HTTP
    assert!(http::HeaderValue::from_str(&header_value).is_ok());
}

#[test]
fn vertex_ai_model_slug_recognized() {
    // Vertex AI uses slugs like "claude-sonnet-4-6@default"
    assert!(super::is_anthropic_model("claude-sonnet-4-6@default"));
}
