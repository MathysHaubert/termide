//! The sections of the AI menu.

/// A section of the AI menu: what its rows list and what can be done to them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiSection {
    /// Agent directories (`agents/<name>/`).
    Agents,
    /// The project's session logs.
    Sessions,
    /// Skill directories (`skills/<name>/SKILL.md`).
    Skills,
    /// Prompt templates (`prompts/<name>.md`).
    Prompts,
}

impl AiSection {
    /// Every section, in the order the AI menu lists them.
    pub const ALL: [Self; 4] = [Self::Agents, Self::Sessions, Self::Skills, Self::Prompts];

    /// The key of the section's row in the AI menu.
    pub fn key(self) -> &'static str {
        match self {
            Self::Agents => "agents",
            Self::Sessions => "sessions",
            Self::Skills => "skills",
            Self::Prompts => "prompts",
        }
    }

    /// The section a menu row's key names.
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|section| section.key() == key)
    }

    /// Its position in the AI menu.
    pub fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|&section| section == self)
            .expect("ALL holds every section")
    }

    /// Whether an item is one file (a prompt, a session log) rather than a
    /// directory (an agent, a skill).
    pub fn item_is_file(self) -> bool {
        matches!(self, Self::Prompts | Self::Sessions)
    }
}

#[cfg(test)]
mod tests {
    use super::AiSection;

    #[test]
    fn keys_and_indices_round_trip() {
        for (index, section) in AiSection::ALL.into_iter().enumerate() {
            assert_eq!(AiSection::from_key(section.key()), Some(section));
            assert_eq!(section.index(), index);
        }
        assert_eq!(AiSection::from_key("browser"), None);
    }
}
