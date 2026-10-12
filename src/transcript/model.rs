//! The common record model every transcript format is parsed into, and
//! everything downstream — main line, collapse rules, the cursor, `read`'s
//! NDJSON, the sessions browser's transcript pane — is written against.
//!
//! One JSONL line can hold several content blocks (a user line carrying two
//! tool results; older Claude versions put text and a tool call in one
//! assistant line), so a line yields **several records sharing the line's
//! uuid**. The tree, the main line and the cursor are made of lines (nodes of
//! the offset index), which keeps them block-agnostic; records only exist
//! once a line on the main line is actually read.

use serde::Serialize;
use serde_json::Value;

#[derive(Serialize, Clone, Debug, PartialEq)]
pub(crate) struct Record {
    pub(crate) uuid: String,
    pub(crate) parent_uuid: Option<String>,
    /// As the transcript writes it (RFC 3339 UTC for Claude).
    pub(crate) timestamp: Option<String>,
    #[serde(flatten)]
    pub(crate) kind: Kind,
}

/// What a record says. Serialized with a `kind` tag — `{"kind":"user",
/// "text":…}` — flat beside the uuids: the NDJSON `read --json` prints.
#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Kind {
    /// The user's words (or what stands in for them: a slash command, the
    /// summary a compaction continues from). Attachments inline as
    /// `[image]`, `[document]`.
    User {
        text: String,
    },
    Assistant {
        text: String,
    },
    Thinking {
        text: String,
    },
    ToolUse {
        /// The call's id, which its result names.
        id: String,
        name: String,
        input: Value,
        /// Set when the call spawned a real subagent (Claude's `Agent`/`Task`).
        #[serde(skip_serializing_if = "Option::is_none")]
        subagent: Option<Subagent>,
    },
    ToolResult {
        tool_use_id: String,
        text: String,
        /// `text`'s length in bytes — what a collapsed result shows, and
        /// what tells a caller whether it is worth reading.
        bytes: usize,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
    },
}

/// A subagent anchored at the tool call that spawned it.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub(crate) struct Subagent {
    /// `Explore`, `general-purpose`, …
    pub(crate) agent_type: String,
    pub(crate) description: String,
    /// Its own transcript, when it could be found.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) transcript: Option<String>,
}
