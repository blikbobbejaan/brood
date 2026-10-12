//! The Rust half of `nest mcp`: the reference documents this binary embeds.
//!
//! The server itself — transport, dispatch, JSON, error mapping, the tool catalogue — is
//! Brood (`std/tool/mcp.blsp`, routed like every other Brood subcommand). What only the
//! binary can supply is the text it compiled in: the docs an agent reads as MCP resources,
//! the prelude source, and the `brood-task` prompt. They reach the server as the dynamic
//! `*mcp-documents*`, bound around the dispatcher call.

use brood::introspect::escape_brood_string;

/// `(name text)` for every document the server serves, keyed as `std/tool/mcp.blsp`'s
/// resource table and `prompts/get` name them.
const DOCUMENTS: &[(&str, &str)] = &[
    (
        "brood-for-claude",
        include_str!("../../../docs/brood-for-claude.md"),
    ),
    (
        "incarnations",
        include_str!("../../../docs/incarnations.md"),
    ),
    ("llm-native", include_str!("../../../docs/llm-native.md")),
    (
        "claude-demo-findings",
        include_str!("../../../docs/claude-demo-findings.md"),
    ),
    ("language", include_str!("../../../docs/language.md")),
    ("decisions", include_str!("../../../docs/decisions.md")),
    ("types", include_str!("../../../docs/types.md")),
    ("error-codes", include_str!("../../../docs/error-codes.md")),
    ("prelude", brood::boot::PRELUDE),
    (
        "brood-task",
        include_str!("../../../docs/prompts/brood-task.md"),
    ),
];

/// `call` (the `(nest/main …)` form) with `*mcp-documents*` bound to the embedded texts.
/// `mcp` is loaded first so its `defdyn` declares the variable before it is bound.
pub fn with_embedded_documents(call: &str) -> String {
    let entries: Vec<String> = DOCUMENTS
        .iter()
        .map(|(name, text)| format!("\"{name}\" \"{}\"", escape_brood_string(text)))
        .collect();
    format!(
        "(do (require-one 'mcp) (binding (*mcp-documents* {{{}}}) {call}))",
        entries.join(" ")
    )
}
