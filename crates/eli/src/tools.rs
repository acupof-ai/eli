//! Tool registry and helpers for the eli crate.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;

use nexil::Tool;

/// Central tool registry. Tools are registered here by the builtin module on
/// startup, but it is a live store: plugins may register tools after
/// init. The model-facing snapshot ([`model_tools_cached`]) is memoized against
/// a fingerprint of this map, so any mutation here is reflected automatically —
/// REGISTRY is the single source of truth.
pub static REGISTRY: std::sync::LazyLock<Mutex<HashMap<String, Tool>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// `(registry fingerprint, model-ready snapshot)` held by [`MODEL_TOOLS_CACHE`].
type ModelToolsSnapshot = (u64, Arc<Vec<Tool>>);

/// Memoized model-ready tools (names rewritten with underscores), keyed by a
/// fingerprint of the REGISTRY contents. Rebuilt only when the registry changes,
/// so the common steady-state read is a cheap fingerprint compare rather than a
/// full rebuild — while never going stale (ARCHITECTURE_REVIEW M3).
static MODEL_TOOLS_CACHE: std::sync::LazyLock<Mutex<Option<ModelToolsSnapshot>>> =
    std::sync::LazyLock::new(|| Mutex::new(None));

/// Order-independent fingerprint of the registry's tool set. Folds each tool's
/// name + description into a commutative accumulator (so HashMap iteration order
/// does not matter) and mixes in the count. Catches additions, removals,
/// renames, and description edits — the cases that change the model-facing list.
fn registry_fingerprint(reg: &HashMap<String, Tool>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut acc = reg.len() as u64;
    for (name, tool) in reg {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        name.hash(&mut h);
        tool.description.hash(&mut h);
        acc = acc.wrapping_add(h.finish());
    }
    acc
}

/// Warm the model-tools cache from the current REGISTRY contents. Called once at
/// startup after `register_builtin_tools()` so the first real turn does not pay
/// the rebuild cost. Safe to call repeatedly; the cache self-invalidates on any
/// later registry mutation.
pub fn populate_model_tools_cache() {
    let _ = model_tools_cached();
}

/// Return the model-ready tool list, reflecting the *current* REGISTRY. Memoized
/// against a registry fingerprint: a hit returns the cached snapshot, a miss (or
/// a registry mutation since the last call) rebuilds and re-caches. This keeps a
/// single source of truth (the live REGISTRY) regardless of how tools were
/// registered (builtin or plugin, before or after init).
pub fn model_tools_cached() -> Vec<Tool> {
    let reg = REGISTRY.lock();
    let fp = registry_fingerprint(&reg);
    if let Some((cached_fp, tools)) = MODEL_TOOLS_CACHE.lock().as_ref()
        && *cached_fp == fp
    {
        let snapshot = Arc::clone(tools);
        drop(reg);
        return snapshot.as_ref().clone();
    }
    // Stale or empty: rebuild from the live registry while holding its lock so
    // the fingerprint and contents stay consistent.
    let built = Arc::new(model_tools(&reg.values().cloned().collect::<Vec<_>>()));
    drop(reg);
    *MODEL_TOOLS_CACHE.lock() = Some((fp, Arc::clone(&built)));
    built.as_ref().clone()
}

/// Convert a tool name with dots to underscore-separated form for model APIs.
fn to_model_name(name: &str) -> String {
    name.replace('.', "_")
}

/// Produce a list of tools with names converted for model consumption.
pub fn model_tools(tools: &[Tool]) -> Vec<Tool> {
    tools
        .iter()
        .map(|tool| {
            let mut cloned = tool.clone();
            cloned.name = to_model_name(&cloned.name);
            cloned
        })
        .collect()
}

/// Shorten a text string for logging.
pub fn shorten_text(text: &str, width: usize) -> String {
    if text.len() <= width {
        return text.to_owned();
    }
    let placeholder = "...";
    let available = width.saturating_sub(placeholder.len());
    if available == 0 {
        return placeholder.to_owned();
    }
    format!("{}{placeholder}", &text[..available])
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexil::Tool;
    use serde_json::json;

    fn make_tool(name: &str, description: &str) -> Tool {
        Tool::schema_only(name, description, json!({}))
    }

    // -- to_model_name --------------------------------------------------------

    #[test]
    fn test_to_model_name_replaces_dots() {
        assert_eq!(to_model_name("tests.rename_me"), "tests_rename_me");
    }

    #[test]
    fn test_to_model_name_no_dots() {
        assert_eq!(to_model_name("simple"), "simple");
    }

    #[test]
    fn test_to_model_name_multiple_dots() {
        assert_eq!(to_model_name("a.b.c"), "a_b_c");
    }

    // -- model_tools ----------------------------------------------------------

    #[test]
    fn test_model_tools_rewrites_names_without_mutating_original() {
        let tool = make_tool("tests.rename_me", "rename");
        let rewritten = model_tools(std::slice::from_ref(&tool));
        assert_eq!(rewritten.len(), 1);
        assert_eq!(rewritten[0].name, "tests_rename_me");
        // Original should be unchanged
        assert_eq!(tool.name, "tests.rename_me");
    }

    #[test]
    fn test_model_tools_empty() {
        let rewritten = model_tools(&[]);
        assert!(rewritten.is_empty());
    }

    // -- REGISTRY -------------------------------------------------------------

    /// Regression for ARCHITECTURE_REVIEW M3: a tool registered into the live
    /// REGISTRY *after* the model-tools cache is primed must still be visible to
    /// the model. With the old `OnceLock` cache this failed silently (stale
    /// snapshot); the fingerprint-memoized cache auto-invalidates on any mutation.
    #[test]
    fn model_tools_cache_reflects_post_init_registration() {
        let probe = "tier1.cache_probe_postinit";
        let model_name = "tier1_cache_probe_postinit";

        // Prime the cache, exactly as startup does after register_builtin_tools().
        populate_model_tools_cache();
        let before = model_tools_cached();
        assert!(
            !before.iter().any(|t| t.name == model_name),
            "probe tool must not exist before registration"
        );

        // A plugin registers a tool into the live REGISTRY after init.
        REGISTRY
            .lock()
            .insert(probe.to_string(), make_tool(probe, "probe"));

        // The model-facing tool list must reflect it (dot rewritten to underscore).
        let after = model_tools_cached();
        let visible = after.iter().any(|t| t.name == model_name);

        // Clean up global state before asserting so a failure doesn't leak.
        REGISTRY.lock().remove(probe);

        assert!(
            visible,
            "post-init registered tool must appear in model_tools_cached()"
        );
    }

    #[test]
    fn test_registry_insert_and_lookup() {
        let tool = make_tool("test.registry_tool", "a tool");
        {
            let mut reg = REGISTRY.lock();
            reg.insert("test.registry_tool".into(), tool.clone());
        }
        let reg = REGISTRY.lock();
        assert!(reg.contains_key("test.registry_tool"));
        assert_eq!(reg["test.registry_tool"].name, "test.registry_tool");
    }

    // -- shorten_text ---------------------------------------------------------

    #[test]
    fn test_shorten_text_short_enough() {
        assert_eq!(shorten_text("hello", 10), "hello");
    }

    #[test]
    fn test_shorten_text_truncates_with_ellipsis() {
        assert_eq!(shorten_text("hello world", 8), "hello...");
    }

    #[test]
    fn test_shorten_text_very_small_width() {
        assert_eq!(shorten_text("hello", 3), "...");
    }

    #[test]
    fn test_shorten_text_zero_width() {
        assert_eq!(shorten_text("hello", 0), "...");
    }
}
