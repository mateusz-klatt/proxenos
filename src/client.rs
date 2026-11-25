//! The agent CLIs that can ask Proxenos for a delegation.

/// A client that `proxenos mcp --client <name>` serves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Client {
    Claude,
    Codex,
    Antigravity,
    Kimi,
    Grok,
    Cursor,
    Copilot,
}

impl Client {
    pub const ALL: [Self; 7] = [
        Self::Claude,
        Self::Codex,
        Self::Antigravity,
        Self::Kimi,
        Self::Grok,
        Self::Cursor,
        Self::Copilot,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Antigravity => "antigravity",
            Self::Kimi => "kimi",
            Self::Grok => "grok",
            Self::Cursor => "cursor",
            Self::Copilot => "copilot",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|client| client.name() == name)
    }
}
