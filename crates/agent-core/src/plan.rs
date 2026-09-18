//! Plan mode's texts: the instructions appended to the system prompt while
//! the mode is on, and the request that carries the plan out once the user
//! accepts it. Both come from `system/plan.md` in the agent directory, the
//! seed being a data file in `assets/`, the way the compaction prompts are.

use crate::layers::split_front_matter;

/// The seed of `system/plan.md`.
pub const SEED_PLAN: &str = include_str!("../assets/system/plan.md");

/// What plan mode tells the model, and what accepting the plan sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanPrompt {
    /// Appended to the system prompt while plan mode is on.
    pub instructions: String,
    /// The user turn sent when the plan is accepted.
    pub request: String,
}

impl Default for PlanPrompt {
    fn default() -> Self {
        Self::from_file(SEED_PLAN)
    }
}

impl PlanPrompt {
    /// Parse `plan.md`: front matter `request:` plus the instructions.
    #[must_use]
    pub fn from_file(text: &str) -> Self {
        let (fields, body) = split_front_matter(text);
        Self {
            instructions: body.trim().to_string(),
            request: fields.get("request").cloned().unwrap_or_default(),
        }
    }

    /// `base` with the plan-mode instructions after it; `base` alone when
    /// the instructions are empty.
    #[must_use]
    pub fn apply(&self, base: &str) -> String {
        if self.instructions.is_empty() {
            return base.to_string();
        }
        if base.trim().is_empty() {
            return self.instructions.clone();
        }
        format!("{}\n\n{}", base.trim_end(), self.instructions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_seed_parses_and_is_appended_to_the_prompt() {
        let plan = PlanPrompt::default();
        assert!(plan.request.starts_with("Carry out the plan"));
        assert!(plan.instructions.starts_with("# Plan mode"));
        assert!(!plan.instructions.contains("request:"));
        let full = plan.apply("You are an agent.\n");
        assert!(full.starts_with("You are an agent.\n\n# Plan mode"));
        assert_eq!(plan.apply(""), plan.instructions);

        let bare = PlanPrompt::from_file("Only instructions.");
        assert_eq!(bare.request, "");
        assert_eq!(bare.apply("base"), "base\n\nOnly instructions.");
        let empty = PlanPrompt::from_file("---\nrequest: go\n---\n");
        assert_eq!(empty.apply("base"), "base");
    }
}
