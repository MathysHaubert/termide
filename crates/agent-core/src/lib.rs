//! Provider-agnostic core of the termide coding agent.
//!
//! The crate owns the pieces that do not depend on a model vendor or on the
//! UI: the transcript types ([`Message`]), the [`Provider`] and [`Tool`]
//! contracts, the agent loop ([`Agent`]) with its steering and follow-up
//! queues, and a thread-backed [`AgentRuntime`] that panels poll from their
//! `tick()` like every other background pipeline in termide.
//!
//! Design rules:
//!
//! - A provider stream never fails. Errors and aborts are encoded in the
//!   final [`AssistantMessage`] through [`StopReason`], so the loop has one
//!   code path for "the model answered".
//! - One turn is one assistant message plus the tool calls it requested.
//!   Steering messages are delivered after the tool batch of the finished
//!   turn and before the next model call; follow-up messages only when the
//!   agent would otherwise stop.
//! - Tool calls can be vetoed by [`Hooks::before_tool_call`]; that is where a
//!   permission prompt plugs in.

pub mod agent;
pub mod cancel;
pub mod compaction;
pub mod context;
pub mod hooks;
pub mod layers;
pub mod mcp;
pub mod message;
pub mod permissions;
pub mod provider;
pub mod runtime;
pub mod session;
pub mod tool;

pub use agent::{
    Agent, AgentConfig, AgentEvent, ChainedHooks, Hooks, NoHooks, QueueHandle, QueueMode,
    ToolDecision,
};
pub use cancel::CancelToken;
pub use compaction::{CompactionPolicy, CompactionReason};
pub use context::{
    build_system_prompt, civil_date, discover_context_files, ContextFile, PromptOptions,
    SEED_TEMPLATE,
};
pub use hooks::{HookConfig, HookEvent, HOOKS_FILE};
pub use layers::{
    ensure_global_layout, split_front_matter, AgentDefinition, AgentDirs, AgentSpec,
    PromptTemplate, SkillInfo, DEFAULT_AGENT, GLOBAL_AGENT_DIR, PROJECT_AGENT_DIR, PROMPTS_DIR,
    ROOT_SOUL_FILE, SESSIONS_DIR, SHARED_SKILLS_DIR, SKILLS_DIR, SKILL_FILE, SOUL_FILE, SPEC_FILE,
};
pub use mcp::{expand_env, McpServerConfig, MCP_FILE};
pub use message::{
    now_millis, AssistantContent, AssistantMessage, Message, StopReason, ToolCall,
    ToolResultContent, ToolResultMessage, Usage, UserContent, UserMessage,
};
pub use permissions::{
    Decision, Mode, ModeHandle, PermissionAnswer, PermissionHooks, PermissionPrompter,
    PermissionRequest, PermissionRules, PersistRule,
};
pub use provider::{ModelInfo, ModelSpec, Provider, Request, StreamEvent, ThinkingLevel, ToolSpec};
pub use runtime::{AgentRuntime, PromptError};
pub use session::{Entry, EntryKind, Session, SessionHeader, SessionModel, SessionSummary};
pub use tool::{LateTools, Tool, ToolContext, ToolRegistry, ToolUpdate};
