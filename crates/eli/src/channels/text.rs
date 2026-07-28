//! Pure text-shaping helpers shared by native channels.
//!
//! Ported verbatim from the deleted `sidecar/plugins/feishu-cli.ts` (commit
//! `ecfa7c3^`). All functions here are pure and unit-tested against the
//! original TypeScript test vectors — no I/O, no channel state.

/// Long-reply chunk cap, interpreted as UTF-8 **bytes** (not chars).
pub const MAX_CHUNK_BYTES: usize = 25000;

/// Count how many leading `char`s of `s` fit within `cap_bytes` UTF-8 bytes.
/// Mirrors the TS `charsFittingBytes` (which counts UTF-16 units, identical for
/// BMP text — all ASCII/CJK inputs the tests exercise).
fn chars_fitting_bytes(chars: &[char], cap_bytes: usize) -> usize {
    let mut bytes = 0usize;
    for (i, c) in chars.iter().enumerate() {
        let cb = c.len_utf8();
        if bytes + cb > cap_bytes {
            return i;
        }
        bytes += cb;
    }
    chars.len()
}

/// Greatest index `i <= from` (in `char` units) where `chars[i..]` starts with
/// `needle`, or `None`. Equivalent to JS `String.lastIndexOf(needle, from)`.
fn last_index_of(chars: &[char], needle: &[char], from: usize) -> Option<usize> {
    if needle.is_empty() {
        return Some(from.min(chars.len()));
    }
    let max_start = from.min(chars.len().saturating_sub(needle.len()));
    (0..=max_start)
        .rev()
        .find(|&i| chars[i..].starts_with(needle))
}

/// If `prefix` ends inside an unclosed ``` fence, return its language tag
/// (possibly empty); otherwise `None`. An odd number of ```-runs means a fence
/// is open. The tag is the run of chars after the backticks that are neither
/// newline nor backtick.
fn unclosed_fence_tag(prefix: &str) -> Option<String> {
    let mut tags: Vec<String> = Vec::new();
    let bytes = prefix.as_bytes();
    let mut i = 0;
    while i + 3 <= bytes.len() {
        if &bytes[i..i + 3] == b"```" {
            let mut j = i + 3;
            while j < bytes.len() && bytes[j] != b'\n' && bytes[j] != b'`' {
                j += 1;
            }
            // Language tag is ASCII in practice; slice on the byte range which
            // is guaranteed a char boundary (backtick + [^\n`]* are all ASCII
            // until a multibyte char, which ends the tag anyway).
            tags.push(prefix[i + 3..j].to_owned());
            i = j;
        } else {
            i += 1;
        }
    }
    if tags.len() % 2 == 1 {
        tags.pop()
    } else {
        None
    }
}

/// Strip leading whitespace (`\s`/`\n`) from a char slice, returning the rest.
fn strip_leading_ws(chars: &[char]) -> &[char] {
    let mut start = 0;
    while start < chars.len() && (chars[start].is_whitespace()) {
        start += 1;
    }
    &chars[start..]
}

/// Split a reply into chunks each `<= cap_bytes` UTF-8 bytes, preferring
/// paragraph > newline > space boundaries but only in the upper half of the
/// window, and never severing an open ``` code fence (the fence is closed and
/// its language tag carried into the next chunk).
pub fn chunk_text(text: &str, cap_bytes: usize) -> Vec<String> {
    if text.len() <= cap_bytes {
        return vec![text.to_owned()];
    }

    let para: Vec<char> = "\n\n".chars().collect();
    let nl: Vec<char> = "\n".chars().collect();
    let sp: Vec<char> = " ".chars().collect();

    let mut chunks: Vec<String> = Vec::new();
    let mut remaining: Vec<char> = text.chars().collect();
    let mut carry_fence: Option<String> = None;

    while remaining.iter().map(|c| c.len_utf8()).sum::<usize>() > cap_bytes {
        let prefix = match &carry_fence {
            Some(tag) => format!("```{tag}\n"),
            None => String::new(),
        };
        let inner_cap = cap_bytes.saturating_sub(prefix.len());
        let max_chars = chars_fitting_bytes(&remaining, inner_cap.max(1));
        let lower_bound = max_chars / 2;

        let cut = last_index_of(&remaining, &para, max_chars)
            .filter(|&c| c >= lower_bound)
            .or_else(|| last_index_of(&remaining, &nl, max_chars).filter(|&c| c >= lower_bound))
            .or_else(|| last_index_of(&remaining, &sp, max_chars).filter(|&c| c >= lower_bound))
            .unwrap_or(max_chars);

        let mut body: String = remaining[..cut].iter().collect();
        match unclosed_fence_tag(&format!("{prefix}{body}")) {
            Some(tag) => {
                body.push_str("\n```");
                carry_fence = Some(tag);
            }
            None => carry_fence = None,
        }
        chunks.push(format!("{prefix}{body}"));
        remaining = strip_leading_ws(&remaining[cut..]).to_vec();
    }

    if !remaining.is_empty() {
        let prefix = match &carry_fence {
            Some(tag) => format!("```{tag}\n"),
            None => String::new(),
        };
        let tail: String = remaining.iter().collect();
        chunks.push(format!("{prefix}{tail}"));
    }
    chunks
}

/// Peel Feishu `@`-mention noise from the **leading edge only**; never touch
/// inline `@`references. Removes `<at>…</at>` and `<at …/>` tags, then up to 3
/// leading `@token ` runs, then trims.
pub fn strip_mentions(text: &str) -> String {
    let mut s = remove_at_tags(text);

    // Peel up to 3 leading "@<non-space> <space>*" tokens.
    for _ in 0..3 {
        let Some(rest) = peel_leading_mention(&s) else {
            break;
        };
        s = rest;
    }
    s.trim().to_owned()
}

/// Remove `<at …>…</at>` (paired, first) then `<at …/>` (self-closing) tags,
/// each with trailing whitespace. Order matters — paired before self-closing.
fn remove_at_tags(text: &str) -> String {
    let paired = remove_tag(text, |t| {
        // <at\s+[^>]*>[^<]*</at>\s*
        let rest = t.strip_prefix("<at")?;
        let after_ws = strip_ws1(rest)?; // requires >=1 whitespace
        let close = after_ws.find('>')?;
        let attrs = &after_ws[..close];
        if attrs.contains('>') {
            return None;
        }
        let inner_start = &after_ws[close + 1..];
        let inner_end = inner_start.find('<')?;
        let after_inner = &inner_start[inner_end..];
        let after_close = after_inner.strip_prefix("</at>")?;
        Some(t.len() - after_close.len() + trim_leading_ws_len(after_close))
    });
    remove_tag(&paired, |t| {
        // <at\s+[^/>]*/>\s*
        let rest = t.strip_prefix("<at")?;
        let after_ws = strip_ws1(rest)?;
        let slash = after_ws.find("/>")?;
        let attrs = &after_ws[..slash];
        if attrs.contains('/') || attrs.contains('>') {
            return None;
        }
        let after = &after_ws[slash + 2..];
        Some(t.len() - after.len() + trim_leading_ws_len(after))
    })
}

/// Scan `text` for the first position where `matcher` returns the end index of
/// a match (relative to that position), removing all matches. `matcher` is
/// given the suffix starting at each index and returns the match length there.
fn remove_tag(text: &str, matcher: impl Fn(&str) -> Option<usize>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut idx = 0;
    while idx < text.len() {
        if text.is_char_boundary(idx)
            && let Some(match_len) = matcher(&text[idx..])
        {
            idx += match_len;
            continue;
        }
        // Copy one char.
        let ch = text[idx..].chars().next().unwrap();
        out.push(ch);
        idx += ch.len_utf8();
    }
    out
}

/// Strip a `@\S+\s*` leading mention; return the remainder, or `None` if the
/// string does not start with `@` followed by a non-whitespace token.
fn peel_leading_mention(s: &str) -> Option<String> {
    let rest = s.strip_prefix('@')?;
    let token_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    if token_end == 0 {
        return None; // "@" followed immediately by whitespace: not \S+
    }
    let after_token = &rest[token_end..];
    let trimmed = after_token.trim_start();
    Some(trimmed.to_owned())
}

/// Require >=1 leading whitespace char; return the suffix after it, else `None`.
fn strip_ws1(s: &str) -> Option<&str> {
    let trimmed = s.trim_start();
    if trimmed.len() == s.len() {
        None
    } else {
        Some(trimmed)
    }
}

/// Byte length of leading whitespace in `s`.
fn trim_leading_ws_len(s: &str) -> usize {
    s.len() - s.trim_start().len()
}

/// Undo double-escaped whitespace (literal `\n`/`\t`/`\r`) only when
/// unambiguous: no real LF present AND at least one literal escape exists.
/// Returns `(text, changed)`.
pub fn normalize_escaped_whitespace(text: &str) -> (String, bool) {
    let has_real_lf = text.contains('\n');
    let has_literal_escape = has_escape_seq(text);
    if has_real_lf || !has_literal_escape {
        return (text.to_owned(), false);
    }
    let next = text
        .replace("\\r\\n", "\n")
        .replace("\\n", "\n")
        .replace("\\r", "\r")
        .replace("\\t", "\t");
    (next, true)
}

/// True if `text` contains a backslash followed by one of `n`/`t`/`r` (JS
/// `/\\[ntr]/`).
fn has_escape_seq(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes
        .windows(2)
        .any(|w| w[0] == b'\\' && matches!(w[1], b'n' | b't' | b'r'))
}

/// Combine multiple message texts into numbered `[消息 i/N] …` lines joined by
/// newline (used for debounced batches of 2+ messages).
pub fn combine_lines(texts: &[&str]) -> String {
    let n = texts.len();
    texts
        .iter()
        .enumerate()
        .map(|(i, t)| format!("[消息 {}/{}] {}", i + 1, n, t))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Rewrite a raw `[Error: … run_model …]` tool error into a friendly Chinese
/// message. Returns the input unchanged unless it starts with `[Error:` and
/// mentions `run_model`.
pub fn friendlyize_error(text: &str) -> String {
    if !(text.starts_with("[Error:") && text.contains("run_model")) {
        return text.to_owned();
    }
    let lower = text.to_lowercase();
    if lower.contains("usage_limit") || lower.contains("rate") || lower.contains("429") {
        "抱歉，当前模型限流了，过会儿再试一下。".to_owned()
    } else if lower.contains("context") && lower.contains("overflow") {
        "对话太长超出了模型上下文窗口，换条新对话再问吧。".to_owned()
    } else if lower.contains("timeout") || lower.contains("timed out") {
        "模型响应超时了，再发一次试试。".to_owned()
    } else {
        "抱歉，模型这次没回上来，过会儿再试一下。".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- chunk_text -----------------------------------------------------------

    #[test]
    fn chunk_short_text_single() {
        assert_eq!(chunk_text("hello", MAX_CHUNK_BYTES), vec!["hello"]);
    }

    #[test]
    fn chunk_paragraph_boundaries() {
        let p1 = format!("{}x", "a".repeat(80)); // 81
        let p2 = format!("{}y", "b".repeat(80)); // 81
        let p3 = "c".repeat(40); // 40
        let text = format!("{p1}\n\n{p2}\n\n{p3}");
        let chunks = chunk_text(&text, 100);
        assert_eq!(chunks, vec![p1, p2, p3]);
    }

    #[test]
    fn chunk_hard_cut_ascii() {
        let chunks = chunk_text(&"x".repeat(200), 50);
        assert_eq!(chunks.len(), 4);
        for c in &chunks {
            assert_eq!(c.len(), 50);
        }
    }

    #[test]
    fn chunk_byte_cap_cjk() {
        let text = "中".repeat(100); // 3 bytes each = 300 bytes
        let chunks = chunk_text(&text, 120);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].chars().count(), 40);
        assert_eq!(chunks[1].chars().count(), 40);
        assert_eq!(chunks[2].chars().count(), 20);
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn chunk_fence_safety() {
        let text = format!("```python\n{}```", "print('x')\n".repeat(20));
        let chunks = chunk_text(&text, 80);
        assert!(chunks.len() >= 2);
        // Every non-final chunk ends with a closing fence.
        for c in &chunks[..chunks.len() - 1] {
            assert!(c.trim_end().ends_with("```"), "chunk missing close: {c:?}");
        }
        // Every non-first chunk reopens with the carried language tag.
        for c in &chunks[1..] {
            assert!(c.starts_with("```python"), "chunk missing reopen: {c:?}");
        }
    }

    // -- strip_mentions -------------------------------------------------------

    #[test]
    fn strip_no_mention_unchanged() {
        assert_eq!(strip_mentions("你好，能帮我查个东西吗"), "你好，能帮我查个东西吗");
    }

    #[test]
    fn strip_one_leading_mention() {
        assert_eq!(strip_mentions("@小助手 你好"), "你好");
    }

    #[test]
    fn strip_two_stacked_mentions() {
        assert_eq!(strip_mentions("@小助手 @secondary 帮我看个东西"), "帮我看个东西");
    }

    #[test]
    fn strip_inline_mention_preserved() {
        assert_eq!(
            strip_mentions("能找一下 @张三 提到的那个文档吗"),
            "能找一下 @张三 提到的那个文档吗"
        );
    }

    #[test]
    fn strip_paired_at_tag() {
        assert_eq!(
            strip_mentions("<at user_id=\"ou_xxx\" user_name=\"bot\">@bot</at> 你好"),
            "你好"
        );
    }

    #[test]
    fn strip_self_closing_at_tag() {
        assert_eq!(strip_mentions("<at user_id=\"ou_xxx\"/>你好"), "你好");
    }

    // -- normalize_escaped_whitespace -----------------------------------------

    #[test]
    fn normalize_real_lf_unchanged() {
        let (t, changed) = normalize_escaped_whitespace("line1\nline2");
        assert_eq!(t, "line1\nline2");
        assert!(!changed);
    }

    #[test]
    fn normalize_literal_backslash_n() {
        let (t, changed) = normalize_escaped_whitespace("line1\\nline2\\nline3");
        assert_eq!(t, "line1\nline2\nline3");
        assert!(changed);
    }

    #[test]
    fn normalize_literal_t_and_r() {
        let (t, changed) = normalize_escaped_whitespace("a\\tb\\rc");
        assert_eq!(t, "a\tb\rc");
        assert!(changed);
    }

    #[test]
    fn normalize_crlf_collapses_to_lf() {
        let (t, changed) = normalize_escaped_whitespace("a\\r\\nb");
        assert_eq!(t, "a\nb");
        assert!(changed);
    }

    #[test]
    fn normalize_mixed_real_and_literal_skips() {
        let (_, changed) = normalize_escaped_whitespace("real\nbreak with literal\\nshown");
        assert!(!changed);
    }

    #[test]
    fn normalize_plain_sentence_skips() {
        let (_, changed) = normalize_escaped_whitespace("just a plain sentence.");
        assert!(!changed);
    }

    // -- friendlyize_error ----------------------------------------------------

    #[test]
    fn friendly_passthrough_when_not_error() {
        assert_eq!(friendlyize_error("hello"), "hello");
        assert_eq!(friendlyize_error("[Error: something else]"), "[Error: something else]");
    }

    #[test]
    fn friendly_rate_limit() {
        assert_eq!(
            friendlyize_error("[Error: run_model failed: 429 usage_limit]"),
            "抱歉，当前模型限流了，过会儿再试一下。"
        );
    }

    #[test]
    fn friendly_context_overflow() {
        assert_eq!(
            friendlyize_error("[Error: run_model context overflow]"),
            "对话太长超出了模型上下文窗口，换条新对话再问吧。"
        );
    }

    #[test]
    fn friendly_timeout() {
        assert_eq!(
            friendlyize_error("[Error: run_model timed out]"),
            "模型响应超时了，再发一次试试。"
        );
    }

    #[test]
    fn friendly_generic_fallback() {
        assert_eq!(
            friendlyize_error("[Error: run_model something inexplicable]"),
            "抱歉，模型这次没回上来，过会儿再试一下。"
        );
    }
}
