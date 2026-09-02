//! Agent state detection via terminal tail pattern matching.
//!
//! VENDORED from herdr (see VENDORED.md in this directory). This file is
//! herdr's `src/detect/mod.rs` with the platform-coupled process-identification
//! half removed — seagents launches its agents itself, so it already knows the
//! command; `identify_agent()` on the recorded argv basename is all it needs.

pub mod config;
pub mod manifest;
pub mod manifest_update;

/// The detected state of a terminal pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    /// Agent finished, prompt visible, nothing happening.
    Idle,
    /// Agent is actively working/processing.
    Working,
    /// Agent needs human input and is blocked on a response.
    Blocked,
    /// Plain shell or unrecognized program.
    Unknown,
}

/// Screen-derived agent state plus confidence metadata used for source arbitration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentDetection {
    pub state: AgentState,
    /// True when the current screen is an agent-owned viewer that shows
    /// transcript/history instead of the live prompt state.
    pub skip_state_update: bool,
    /// True when the current screen visibly shows live idle chrome.
    pub visible_idle: bool,
    /// True when the current screen visibly shows live UI chrome that needs
    /// human input. This is stronger than arbitrary prompt-like text in the
    /// scrollback and may override a non-blocked integration state.
    pub visible_blocker: bool,
    /// True when the current screen visibly shows live working chrome. PTY
    /// activity is the normal working authority; this remains diagnostic
    /// metadata and for non-PTY fallback paths.
    pub visible_working: bool,
}

/// Which agent we detected running in a pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    Pi,
    Claude,
    Codex,
    Gemini,
    Cursor,
    Devin,
    Antigravity,
    Cline,
    Omp,
    Mastracode,
    OpenCode,
    GithubCopilot,
    Kimi,
    Kiro,
    Droid,
    Amp,
    Grok,
    Hermes,
    Kilo,
    Qodercli,
    Qwen,
    Maki,
    Muse,
}

impl Agent {
    pub const SCREEN_MANIFEST_AGENTS: [Self; 21] = [
        Self::Pi,
        Self::Claude,
        Self::Codex,
        Self::Gemini,
        Self::Cursor,
        Self::Devin,
        Self::Antigravity,
        Self::Cline,
        Self::OpenCode,
        Self::GithubCopilot,
        Self::Kimi,
        Self::Kiro,
        Self::Droid,
        Self::Amp,
        Self::Grok,
        Self::Hermes,
        Self::Kilo,
        Self::Qodercli,
        Self::Qwen,
        Self::Maki,
        Self::Muse,
    ];
}

pub fn agent_label(agent: Agent) -> &'static str {
    match agent {
        Agent::Pi => "pi",
        Agent::Claude => "claude",
        Agent::Codex => "codex",
        Agent::Gemini => "gemini",
        Agent::Cursor => "cursor",
        Agent::Devin => "devin",
        Agent::Antigravity => "agy",
        Agent::Cline => "cline",
        Agent::Omp => "omp",
        Agent::Mastracode => "mastracode",
        Agent::OpenCode => "opencode",
        Agent::GithubCopilot => "copilot",
        Agent::Kimi => "kimi",
        Agent::Kiro => "kiro",
        Agent::Droid => "droid",
        Agent::Amp => "amp",
        Agent::Grok => "grok",
        Agent::Hermes => "hermes",
        Agent::Kilo => "kilo",
        Agent::Qodercli => "qodercli",
        Agent::Qwen => "qwen",
        Agent::Maki => "maki",
        Agent::Muse => "muse",
    }
}

pub fn parse_agent_label(agent: &str) -> Option<Agent> {
    let name = normalized_agent_lookup_name(agent);
    parse_canonical_agent_label(&name).or_else(|| lookup_agent(&name))
}

pub(crate) fn parse_canonical_agent_label(label: &str) -> Option<Agent> {
    let agent = lookup_agent(label)?;
    (agent_label(agent) == label).then_some(agent)
}

fn lookup_agent(name: &str) -> Option<Agent> {
    let name = path_basename(name);
    match name {
        "pi" => Some(Agent::Pi),
        "claude" | "claude-code" => Some(Agent::Claude),
        "codex" => Some(Agent::Codex),
        "gemini" => Some(Agent::Gemini),
        "cursor" | "cursor-agent" => Some(Agent::Cursor),
        "devin" | "devin-cli" | "devin cli" => Some(Agent::Devin),
        "agy" | "antigravity" | "antigravity-cli" => Some(Agent::Antigravity),
        "cline" => Some(Agent::Cline),
        "omp" => Some(Agent::Omp),
        "mastracode" | "mastra-code" | "mastra code" => Some(Agent::Mastracode),
        "opencode" | "opencode2" | "open-code" => Some(Agent::OpenCode),
        "copilot" | "github-copilot" | "ghcs" => Some(Agent::GithubCopilot),
        "kimi" | "kimi-code" | "kimi code" => Some(Agent::Kimi),
        "kiro" | "kiro-cli" => Some(Agent::Kiro),
        "droid" => Some(Agent::Droid),
        "amp" | "amp-local" => Some(Agent::Amp),
        "grok" | "grok-build" => Some(Agent::Grok),
        "hermes" | "hermes-agent" => Some(Agent::Hermes),
        "kilo" | "kilo-code" | "kilo code" => Some(Agent::Kilo),
        "qodercli" | "qoderclicn" | "qoder" | "qodercn" => Some(Agent::Qodercli),
        "qwen" | "qwen-code" | "qwen code" => Some(Agent::Qwen),
        "maki" => Some(Agent::Maki),
        "muse" | "muse-code" | "muse-cli" => Some(Agent::Muse),
        _ if is_muse_versioned_binary(name) => Some(Agent::Muse),
        _ => None,
    }
}

/// Muse's install-dir launcher script resolves the active release and execs
/// `muse-bin-<version>` (e.g. `muse-bin-0.1.0-R708.1`), so the running
/// process never carries a bare `muse`/`muse-bin` alias. Require a digit
/// immediately after the `muse-bin-` prefix so unrelated binaries such as
/// `muse-binary` or a bare `muse-bin` stay unmatched.
/// Accepts path-qualified `argv0` values by checking only the basename, since
/// the launcher may `exec` with an absolute install-dir path.
fn is_muse_versioned_binary(name: &str) -> bool {
    path_basename(name)
        .strip_prefix("muse-bin-")
        .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
}

/// Identify which agent is running from the process name.
/// Returns `None` for plain shells or unrecognized programs.
pub fn identify_agent(process_name: &str) -> Option<Agent> {
    parse_agent_label(process_name)
}

/// Detect the state of an agent from the live terminal tail snapshot.
/// If `agent` is `None`, returns `Unknown`.
#[cfg(test)]
pub fn detect_state(agent: Option<Agent>, screen_content: &str) -> AgentState {
    detect_agent(agent, screen_content).state
}

/// Detect state and whether a visible blocker is present on the current screen.
#[allow(dead_code)] // shim for existing callers; detect_agent_with_osc is the real path
pub fn detect_agent(agent: Option<Agent>, screen_content: &str) -> AgentDetection {
    detect_agent_with_osc(agent, screen_content, "", "")
}

/// Detect state using screen content plus OSC title/progress strings.
pub fn detect_agent_with_osc(
    agent: Option<Agent>,
    screen_content: &str,
    osc_title: &str,
    osc_progress: &str,
) -> AgentDetection {
    let Some(agent) = agent else {
        return AgentDetection {
            state: AgentState::Unknown,
            skip_state_update: false,
            visible_idle: false,
            visible_blocker: false,
            visible_working: false,
        };
    };
    manifest::detect_with_osc(
        agent,
        manifest::DetectionInput {
            screen: screen_content,
            osc_title,
            osc_progress,
        },
    )
}

#[allow(dead_code)] // seagents: vendored API surface kept for upstream parity
pub fn should_skip_state_update(agent: Option<Agent>, screen_content: &str) -> bool {
    agent.is_some_and(|agent| manifest::should_skip_state_update(agent, screen_content))
}

fn normalized_agent_lookup_name(name: &str) -> String {
    let mut name = name.trim().to_lowercase();
    for suffix in [".exe", ".cmd", ".bat", ".ps1", ".js"] {
        if name.ends_with(suffix) {
            name.truncate(name.len() - suffix.len());
            break;
        }
    }
    name
}

fn path_basename(path: &str) -> &str {
    path.rsplit(['/', '\\'])
        .find(|component| !component.is_empty())
        .unwrap_or(path)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moved_agent_detection_routes_through_production_dispatch() {
        let detection = detect_agent(Some(Agent::Pi), "Working...");

        assert_eq!(detection.state, AgentState::Working);
        assert!(detection.visible_working);
    }

    // ---- Agent identification ----

    #[test]
    fn identify_known_agents() {
        assert_eq!(identify_agent("pi"), Some(Agent::Pi));
        assert_eq!(identify_agent("claude"), Some(Agent::Claude));
        assert_eq!(identify_agent("claude-code"), Some(Agent::Claude));
        assert_eq!(identify_agent("codex"), Some(Agent::Codex));
        assert_eq!(identify_agent("gemini"), Some(Agent::Gemini));
        assert_eq!(identify_agent("cursor"), Some(Agent::Cursor));
        assert_eq!(identify_agent("cursor-agent"), Some(Agent::Cursor));
        assert_eq!(identify_agent("devin"), Some(Agent::Devin));
        assert_eq!(identify_agent("devin-cli"), Some(Agent::Devin));
        assert_eq!(identify_agent("agy"), Some(Agent::Antigravity));
        assert_eq!(identify_agent("antigravity-cli"), Some(Agent::Antigravity));
        assert_eq!(identify_agent("cline"), Some(Agent::Cline));
        assert_eq!(identify_agent("omp"), Some(Agent::Omp));
        assert_eq!(identify_agent("mastracode"), Some(Agent::Mastracode));
        assert_eq!(identify_agent("mastra-code"), Some(Agent::Mastracode));
        assert_eq!(identify_agent("opencode"), Some(Agent::OpenCode));
        assert_eq!(identify_agent("opencode.exe"), Some(Agent::OpenCode));
        assert_eq!(identify_agent("opencode2"), Some(Agent::OpenCode));
        assert_eq!(identify_agent("opencode2.exe"), Some(Agent::OpenCode));
        assert_eq!(identify_agent("kimi"), Some(Agent::Kimi));
        assert_eq!(identify_agent("Kimi Code"), Some(Agent::Kimi));
        assert_eq!(identify_agent("kiro"), Some(Agent::Kiro));
        assert_eq!(identify_agent("kiro-cli"), Some(Agent::Kiro));
        assert_eq!(identify_agent("copilot"), Some(Agent::GithubCopilot));
        assert_eq!(identify_agent("ghcs"), Some(Agent::GithubCopilot));
        assert_eq!(identify_agent("grok"), Some(Agent::Grok));
        assert_eq!(identify_agent("grok-build"), Some(Agent::Grok));
        assert_eq!(identify_agent("hermes"), Some(Agent::Hermes));
        assert_eq!(identify_agent("hermes-agent"), Some(Agent::Hermes));
        assert_eq!(identify_agent("kilo"), Some(Agent::Kilo));
        assert_eq!(identify_agent("kilo-code"), Some(Agent::Kilo));
        assert_eq!(identify_agent("qwen"), Some(Agent::Qwen));
        assert_eq!(identify_agent("Qwen Code"), Some(Agent::Qwen));
        assert_eq!(identify_agent("maki"), Some(Agent::Maki));
        assert_eq!(identify_agent("muse"), Some(Agent::Muse));
        assert_eq!(identify_agent("muse-code"), Some(Agent::Muse));
        assert_eq!(identify_agent("muse-cli"), Some(Agent::Muse));
        assert_eq!(identify_agent("muse-bin-0.1.0-R708.1"), Some(Agent::Muse));
        assert_eq!(identify_agent("muse-bin-1.2.3"), Some(Agent::Muse));
        assert_eq!(
            identify_agent("/home/user/.local/bin/muse-bin-0.2.1-R1215.1"),
            Some(Agent::Muse)
        );
        assert_eq!(
            identify_agent(r"C:\Users\user\muse-bin-0.2.1-R1215.1.exe"),
            Some(Agent::Muse)
        );
    }

    #[test]
    fn parse_known_agent_labels() {
        assert_eq!(parse_agent_label("pi"), Some(Agent::Pi));
        assert_eq!(parse_agent_label("claude"), Some(Agent::Claude));
        assert_eq!(parse_agent_label("cursor-agent"), Some(Agent::Cursor));
        assert_eq!(parse_agent_label("devin-cli"), Some(Agent::Devin));
        assert_eq!(parse_agent_label("agy"), Some(Agent::Antigravity));
        assert_eq!(parse_agent_label("antigravity"), Some(Agent::Antigravity));
        assert_eq!(parse_agent_label("omp"), Some(Agent::Omp));
        assert_eq!(parse_agent_label("mastracode"), Some(Agent::Mastracode));
        assert_eq!(parse_agent_label("mastra code"), Some(Agent::Mastracode));
        assert_eq!(parse_agent_label("opencode.exe"), Some(Agent::OpenCode));
        assert_eq!(parse_agent_label("copilot"), Some(Agent::GithubCopilot));
        assert_eq!(parse_agent_label("kimi-code"), Some(Agent::Kimi));
        assert_eq!(
            parse_agent_label("github-copilot"),
            Some(Agent::GithubCopilot)
        );
        assert_eq!(parse_agent_label("amp-local"), Some(Agent::Amp));
        assert_eq!(parse_agent_label("kiro-cli"), Some(Agent::Kiro));
        assert_eq!(parse_agent_label("grok-build"), Some(Agent::Grok));
        assert_eq!(parse_agent_label("hermes-agent"), Some(Agent::Hermes));
        assert_eq!(parse_agent_label("qwen-code"), Some(Agent::Qwen));
        assert_eq!(parse_agent_label("maki"), Some(Agent::Maki));
        assert_eq!(parse_agent_label("kilo-code"), Some(Agent::Kilo));
    }

    #[test]
    fn canonical_agent_labels_are_strict() {
        assert_eq!(parse_canonical_agent_label("claude-code"), None);
        assert_eq!(parse_canonical_agent_label("Pi"), None);
        assert_eq!(parse_canonical_agent_label(" pi "), None);
        assert_eq!(parse_canonical_agent_label("opencode.exe"), None);
    }

    #[test]
    fn identify_unknown_processes() {
        assert_eq!(identify_agent("bash"), None);
        assert_eq!(identify_agent("zsh"), None);
        assert_eq!(identify_agent("vim"), None);
        assert_eq!(identify_agent("node"), None);
        assert_eq!(identify_agent("museum"), None);
        assert_eq!(identify_agent("muse-helper"), None);
        assert_eq!(identify_agent("muser"), None);
        assert_eq!(identify_agent("musescore"), None);
        assert_eq!(identify_agent("muse-bin"), None);
        assert_eq!(identify_agent("muse-bin-"), None);
        assert_eq!(identify_agent("muse-binary"), None);
    }

    #[test]
    fn identify_case_insensitive() {
        assert_eq!(identify_agent("Pi"), Some(Agent::Pi));
        assert_eq!(identify_agent("CLAUDE"), Some(Agent::Claude));
        assert_eq!(identify_agent("Codex"), Some(Agent::Codex));
        assert_eq!(identify_agent("Devin"), Some(Agent::Devin));
    }

    // ---- Screen detection routing ----

    #[test]
    fn no_agent_returns_unknown() {
        assert_eq!(detect_state(None, "anything"), AgentState::Unknown);
    }
}
