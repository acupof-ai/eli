//! Smart message router: fast-classify inbound messages.
//!
//! Greet messages get an instant canned response (no LLM).
//! Everything else goes through the full Chat pipeline.

use std::collections::HashMap;
use std::sync::LazyLock;

use rand::seq::SliceRandom;

// ---------------------------------------------------------------------------
// Route decision
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Greeting table
// ---------------------------------------------------------------------------

/// (lowercase trigger word, list of 5 canned responses)
static GREETINGS: LazyLock<HashMap<&'static str, &'static [&'static str]>> = LazyLock::new(|| {
    let entries: &[(&str, &[&str])] = &[
        (
            "你好",
            &["在呢！", "嗨~", "你好呀", "有什么事？", "来了来了"],
        ),
        (
            "hello",
            &["Hey!", "What's up?", "Hello!", "Hi there!", "Yo!"],
        ),
        ("hi", &["Hey!", "嗨！", "Hi!", "有什么事？", "在呢"]),
        ("hey", &["Hey!", "嗨~", "Yo!", "What's up?", "有啥事？"]),
        ("嗨", &["嗨！", "在呢~", "你好呀", "嗨嗨", "有什么事？"]),
        (
            "在吗",
            &["在呢！", "在的", "一直在~", "在呢在呢", "有什么事？"],
        ),
        (
            "在不在",
            &["在呢！", "在的", "一直都在", "在呢在呢", "说吧"],
        ),
        (
            "早",
            &["早！", "早上好~", "早安！", "早呀", "早！今天有什么计划？"],
        ),
        (
            "早安",
            &["早安！", "早上好~", "早！", "早安呀", "早！新的一天"],
        ),
        (
            "晚安",
            &["晚安！", "晚安~好梦", "晚安！", "night~", "晚安呀"],
        ),
        (
            "good morning",
            &[
                "Morning!",
                "Good morning!",
                "Hey, morning!",
                "Morning~",
                "Rise and shine!",
            ],
        ),
        (
            "good night",
            &[
                "Night!",
                "Good night!",
                "Sleep well!",
                "Night night!",
                "Sweet dreams!",
            ],
        ),
        (
            "谢谢",
            &["不客气！", "小事~", "没事！", "随时找我", "应该的"],
        ),
        (
            "thanks",
            &[
                "No problem!",
                "Sure thing!",
                "Anytime!",
                "You got it!",
                "Happy to help!",
            ],
        ),
        (
            "thank you",
            &[
                "You're welcome!",
                "No problem!",
                "Anytime!",
                "Sure!",
                "Of course!",
            ],
        ),
        (
            "thx",
            &["No prob!", "Sure!", "Anytime!", "👌", "You got it!"],
        ),
        (
            "bye",
            &["Bye!", "See ya!", "Later!", "拜拜~", "Catch you later!"],
        ),
        ("再见", &["再见！", "拜拜~", "回见！", "下次见~", "拜！"]),
        ("拜拜", &["拜拜！", "回见~", "下次见！", "拜！", "See ya!"]),
        ("ok", &["👌", "好的！", "OK!", "收到", "Got it!"]),
        ("okay", &["👌", "好的！", "OK!", "收到", "Got it!"]),
        ("好的", &["👌", "OK!", "好~", "收到", "没问题"]),
        ("收到", &["👌", "好~", "OK!", "收到收到", "了解"]),
        ("嗯", &["👌", "嗯嗯", "好~", "收到", "了解"]),
    ];
    entries.iter().copied().collect()
});

/// Characters treated as trailing punctuation (stripped before matching).
const PUNCT: &str = "!.?！。？~～";

// ---------------------------------------------------------------------------
// Smart router
// ---------------------------------------------------------------------------

/// Rule-based message classifier.
pub struct SmartRouter;

impl SmartRouter {
    pub fn new() -> Self {
        Self
    }

    /// Classify a message. Returns `Some(reply)` for greetings,
    /// `None` for everything else (default Chat pipeline).
    pub fn classify(&self, content: &str) -> Option<String> {
        let trimmed = content.trim();
        if trimmed.is_empty() {
            return None;
        }

        let lower = trimmed.to_lowercase();
        let stripped = lower.trim_end_matches(|c: char| PUNCT.contains(c));

        if let Some(responses) = GREETINGS.get(stripped) {
            let mut rng = rand::thread_rng();
            let reply = responses.choose(&mut rng).unwrap_or(&responses[0]);
            return Some((*reply).to_owned());
        }

        None
    }
}

impl Default for SmartRouter {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Context key for passing route decisions through the framework
// ---------------------------------------------------------------------------

/// Context key used to pass route decisions through the inbound envelope.
pub const ROUTE_CONTEXT_KEY: &str = "_route";

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn router() -> SmartRouter {
        SmartRouter::new()
    }

    #[test]
    fn test_greet_chinese() {
        let r = router();
        let result = r.classify("你好");
        assert!(matches!(result, Some(_)));
        if let Some(text) = result {
            let valid = ["在呢！", "嗨~", "你好呀", "有什么事？", "来了来了"];
            assert!(valid.contains(&text.as_str()), "unexpected: {text}");
        }
    }

    #[test]
    fn test_greet_english() {
        let r = router();
        let result = r.classify("hello");
        assert!(matches!(result, Some(_)));
    }

    #[test]
    fn test_greet_with_punctuation() {
        let r = router();
        assert!(matches!(r.classify("你好！"), Some(_)));
        assert!(matches!(r.classify("hello!"), Some(_)));
        assert!(matches!(r.classify("hi?"), Some(_)));
        assert!(matches!(r.classify("谢谢！！"), Some(_)));
    }

    #[test]
    fn test_greet_case_insensitive() {
        let r = router();
        assert!(matches!(r.classify("HELLO"), Some(_)));
        assert!(matches!(r.classify("Hello"), Some(_)));
        assert!(matches!(r.classify("Good Morning"), Some(_)));
    }

    #[test]
    fn test_non_greet_returns_none() {
        let r = router();
        assert_eq!(r.classify("帮我看看代码"), None);
        assert_eq!(r.classify("search for files"), None);
        assert_eq!(r.classify("Can you explain this?"), None);
    }

    #[test]
    fn test_command_returns_none() {
        let r = router();
        assert_eq!(r.classify("/help"), None);
        assert_eq!(r.classify("/status"), None);
    }

    #[test]
    fn test_empty_returns_none() {
        let r = router();
        assert_eq!(r.classify(""), None);
        assert_eq!(r.classify("   "), None);
    }

    #[test]
    fn test_all_greet_words_have_five_responses() {
        for (word, responses) in GREETINGS.iter() {
            assert_eq!(
                responses.len(),
                5,
                "greeting '{word}' should have exactly 5 responses, got {}",
                responses.len()
            );
        }
    }
}
