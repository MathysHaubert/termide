//! Built-in tools of the termide coding agent.
//!
//! Four tools cover what a coding agent needs on a local checkout: `read`,
//! `edit`, `write` and `bash`; `skill` joins them when the project or the
//! user defines skills. Search is left to the shell (`rg`, `find`), which
//! every model already knows. Contracts follow the cross-agent
//! comparison in `doc/en/agent-design.md`: numbered lines on read,
//! search/replace with a unique anchor and tolerant whitespace matching on
//! edit, head-and-tail truncation of shell output with the full log saved to
//! a file.

mod args;
mod bash;
mod clean;
mod edit;
mod read;
mod skill;
mod task;
mod truncate;
mod write;

use std::sync::Arc;

use termide_agent_core::ToolRegistry;

pub use bash::BashTool;
pub use clean::{clean_output, Cleaned};
pub use edit::EditTool;
pub use read::ReadTool;
pub use skill::SkillTool;
pub use task::{SubagentRun, TaskTool};
pub use write::WriteTool;

/// The default registry: `read`, `edit`, `write`, `bash`, in prompt order.
#[must_use]
pub fn builtin_tools() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.insert(Arc::new(ReadTool));
    registry.insert(Arc::new(EditTool));
    registry.insert(Arc::new(WriteTool));
    registry.insert(Arc::new(BashTool::default()));
    registry
}
