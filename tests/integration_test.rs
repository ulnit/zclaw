//! Integration tests for ulnclaw agent engine

use serde_json::json;
use ulnclaw::prelude::*;

/// Test tool registry creation and tool registration
#[tokio::test]
async fn test_tool_registry() {
    let mut registry = ToolRegistry::new();

    // Register a simple tool
    let tool1 = tool("echo")
        .description("Echo back the input")
        .parameters(json!({
            "type": "object",
            "properties": {
                "message": {"type": "string"}
            },
            "required": ["message"]
        }))
        .handler(|args, _ctx| async move {
            let msg = args.get("message").and_then(|v| v.as_str()).unwrap_or("");
            Ok(json!({"echo": msg}))
        })
        .toolset("test")
        .build()
        .unwrap();

    registry.register(tool1);

    // Verify tool is registered
    assert!(registry.has("echo"));
    assert_eq!(registry.len(), 1);

    // Get definitions
    let defs = registry.definitions();
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].name, "echo");

    // Dispatch tool
    let result = registry
        .dispatch(
            "echo",
            json!({"message": "hello"}),
            std::sync::Arc::new(ulnclaw::ToolContext::new()),
        )
        .await
        .unwrap();
    assert_eq!(result, json!({"echo": "hello"}));
}

/// Test tool dispatch with error handling
#[tokio::test]
async fn test_tool_dispatch_error() {
    let mut registry = ToolRegistry::new();

    let tool = tool("failing_tool")
        .description("A tool that fails")
        .handler(|_args, _ctx| async move { Err(ulnclaw::AgentError::tool("intentional failure")) })
        .build()
        .unwrap();

    registry.register(tool);

    let result = registry
        .dispatch(
            "failing_tool",
            json!({}),
            std::sync::Arc::new(ulnclaw::ToolContext::new()),
        )
        .await;
    assert!(result.is_err());
}

/// Test toolset enable/disable
#[tokio::test]
async fn test_toolset_management() {
    let mut registry = ToolRegistry::new();

    let tool1 = tool("tool_a")
        .description("Tool A")
        .handler(|_, _ctx| async move { Ok(json!({})) })
        .toolset("group1")
        .build()
        .unwrap();

    let tool2 = tool("tool_b")
        .description("Tool B")
        .handler(|_, _ctx| async move { Ok(json!({})) })
        .toolset("group2")
        .build()
        .unwrap();

    registry.register(tool1);
    registry.register(tool2);

    assert_eq!(registry.definitions().len(), 2);

    // Disable group1
    registry.disable_toolset("group1");
    assert_eq!(registry.definitions().len(), 1);
    assert_eq!(registry.definitions()[0].name, "tool_b");

    // Re-enable group1
    registry.enable_toolset("group1");
    assert_eq!(registry.definitions().len(), 2);
}

/// Test prompt builder
#[test]
fn test_prompt_builder() {
    use ulnclaw::PromptBuilder;

    let prompt = PromptBuilder::new()
        .identity("You are a helpful assistant.")
        .tool_guidance("Use tools when needed.")
        .add_skill("Always be polite.")
        .add_env_hint("OS", "Linux")
        .memory("User prefers dark mode.")
        .build();

    assert!(prompt.contains("You are a helpful assistant."));
    assert!(prompt.contains("Use tools when needed."));
    assert!(prompt.contains("Always be polite."));
    assert!(prompt.contains("Linux"));
    assert!(prompt.contains("User prefers dark mode."));
}

/// Test context compressor
#[test]
fn test_context_compressor() {
    use ulnclaw::provider::{Message, Role};
    use ulnclaw::ContextCompressor;

    let compressor = ContextCompressor::new(100);

    // Small context - no compression needed
    let small_msgs = vec![Message {
        role: Role::User,
        content: Some("Hello".to_string()),
        tool_calls: None,
        tool_call_id: None,
        name: None,
    }];
    assert!(!compressor.needs_compression(&small_msgs));

    // Large context - compression needed
    let large_msg = "x".repeat(1000);
    let large_msgs = vec![Message {
        role: Role::User,
        content: Some(large_msg),
        tool_calls: None,
        tool_call_id: None,
        name: None,
    }];
    assert!(compressor.needs_compression(&large_msgs));
}

/// Test session store
#[test]
fn test_memory_session_store() {
    use ulnclaw::session::{new_session, MemorySessionStore, SessionStore};

    let store = MemorySessionStore::new();

    // Create and save session
    let mut session = new_session("conv-1");
    session.messages.push(Message {
        role: Role::User,
        content: Some("Hello".to_string()),
        tool_calls: None,
        tool_call_id: None,
        name: None,
    });

    store.save_session(&session).unwrap();

    // Load session
    let loaded = store.load_session(&session.id).unwrap();
    assert!(loaded.is_some());
    assert_eq!(loaded.unwrap().messages.len(), 1);

    // List sessions
    let sessions = store.list_sessions(10).unwrap();
    assert_eq!(sessions.len(), 1);

    // Search sessions
    let results = store.search_sessions("Hello", 10).unwrap();
    assert_eq!(results.len(), 1);

    // Delete session
    store.delete_session(&session.id).unwrap();
    let loaded = store.load_session(&session.id).unwrap();
    assert!(loaded.is_none());
}

/// Test thinking block stripping
#[test]
fn test_strip_thinking_blocks() {
    use ulnclaw::agent::strip_thinking_blocks;

    let input = "Before <think>thinking</think> After";
    assert_eq!(strip_thinking_blocks(input), "Before  After");

    let input2 = "No thinking here";
    assert_eq!(strip_thinking_blocks(input2), "No thinking here");

    let input3 = "Start <thinking>hidden</thinking> middle <think>also hidden</think> end";
    assert_eq!(strip_thinking_blocks(input3), "Start  middle  end");
}

/// Test provider config building
#[test]
fn test_provider_config() {
    use ulnclaw::provider::{ProviderConfig, ProviderKind};

    let config = ProviderConfig {
        name: "Test Provider".to_string(),
        kind: ProviderKind::OpenAiCompatible,
        endpoint: "https://api.openai.com".to_string(),
        api_key: Some("sk-test".to_string()),
        model: "gpt-4o".to_string(),
        max_tokens: Some(4096),
        temperature: Some(0.7),
        fallback_providers: vec![],
    };

    let provider = config.build().unwrap();
    assert_eq!(provider.model(), "gpt-4o");
    assert_eq!(provider.name(), "Test Provider");
}

/// Test agent creation (without actual API calls)
#[tokio::test]
async fn test_agent_creation() {
    use ulnclaw::provider::{ProviderConfig, ProviderKind};
    use ulnclaw::AgentConfig;

    let config = ProviderConfig {
        name: "Test".to_string(),
        kind: ProviderKind::OpenAiCompatible,
        endpoint: "https://api.example.com".to_string(),
        api_key: Some("key".to_string()),
        model: "test-model".to_string(),
        max_tokens: None,
        temperature: None,
        fallback_providers: vec![],
    };

    let provider = config.build().unwrap();
    let tools = ToolRegistry::new();

    let agent = Agent::new(std::sync::Arc::from(provider), tools).with_config(AgentConfig {
        max_iterations: 10,
        system_prompt: Some("You are helpful.".to_string()),
        ..Default::default()
    });

    // Agent is created successfully - actual API calls would fail without a real endpoint
    drop(agent);
}

/// `ulnclaw completion <shell>` emits a usable script for each supported
/// shell (hermes completion parity — bash/zsh/fish plus clap_complete
/// extras).
#[test]
fn test_completion_scripts() {
    let bin = env!("CARGO_BIN_EXE_ulnclaw");
    for (shell, needle) in [
        ("bash", "_ulnclaw"),
        ("zsh", "#compdef ulnclaw"),
        ("fish", "complete -c ulnclaw"),
    ] {
        let output = std::process::Command::new(bin)
            .args(["completion", shell])
            .output()
            .expect("run ulnclaw completion");
        assert!(
            output.status.success(),
            "{shell} failed: {:?}",
            output.status
        );
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(text.contains(needle), "{shell} script missing {needle}");
        assert!(text.len() > 500, "{shell} script suspiciously small");
    }
}
