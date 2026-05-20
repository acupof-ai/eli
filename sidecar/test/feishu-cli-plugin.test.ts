import { afterEach, describe, expect, it } from "bun:test";
import {
  __inflightDepth,
  __resetChannelState,
  __resetSeenEventIds,
  __seedInflight,
  __takeInflightForTest,
  alreadySeen,
  chunkText,
  combineEnvelopes,
  friendlyizeError,
  isAppSender,
  MAX_CHUNK_BYTES,
  MAX_CHUNK_CHARS,
  normalizeEscapedWhitespace,
  routeArgs,
  stripMentions,
  type InboundItem,
} from "../plugins/feishu-cli.ts";
import type { InboundEnvelope } from "../src/types.ts";

afterEach(() => {
  __resetSeenEventIds();
  __resetChannelState();
});

// ---------------------------------------------------------------------------
// routeArgs
// ---------------------------------------------------------------------------

describe("routeArgs", () => {
  it("uses --chat-id for oc_ ids", () => {
    expect(routeArgs("oc_abc123")).toEqual(["--chat-id", "oc_abc123"]);
  });

  it("uses --user-id for ou_ ids", () => {
    expect(routeArgs("ou_xyz789")).toEqual(["--user-id", "ou_xyz789"]);
  });

  it("falls back to --chat-id for unknown prefix", () => {
    expect(routeArgs("weird_id")).toEqual(["--chat-id", "weird_id"]);
  });
});

// ---------------------------------------------------------------------------
// alreadySeen — dedup of replayed events
// ---------------------------------------------------------------------------

describe("alreadySeen", () => {
  it("treats first sighting as fresh", () => {
    expect(alreadySeen("evt_a")).toBe(false);
  });

  it("flags subsequent sightings as duplicate", () => {
    expect(alreadySeen("evt_b")).toBe(false);
    expect(alreadySeen("evt_b")).toBe(true);
    expect(alreadySeen("evt_b")).toBe(true);
  });

  it("treats undefined as not-seen but never records it", () => {
    expect(alreadySeen(undefined)).toBe(false);
    expect(alreadySeen(undefined)).toBe(false);
  });

  it("isolates state across event ids", () => {
    expect(alreadySeen("evt_c")).toBe(false);
    expect(alreadySeen("evt_d")).toBe(false);
    expect(alreadySeen("evt_c")).toBe(true);
    expect(alreadySeen("evt_d")).toBe(true);
  });
});

// ---------------------------------------------------------------------------
// normalizeEscapedWhitespace
// ---------------------------------------------------------------------------

describe("normalizeEscapedWhitespace", () => {
  it("leaves real newlines alone", () => {
    const input = "line1\nline2";
    const out = normalizeEscapedWhitespace(input);
    expect(out.changed).toBe(false);
    expect(out.text).toBe(input);
  });

  it("unescapes pure literal \\n strings", () => {
    const out = normalizeEscapedWhitespace("line1\\nline2\\nline3");
    expect(out.changed).toBe(true);
    expect(out.text).toBe("line1\nline2\nline3");
  });

  it("handles \\t and \\r too", () => {
    const out = normalizeEscapedWhitespace("a\\tb\\rc");
    expect(out.changed).toBe(true);
    expect(out.text).toBe("a\tb\rc");
  });

  it("collapses \\r\\n pairs to single LF", () => {
    const out = normalizeEscapedWhitespace("a\\r\\nb");
    expect(out.changed).toBe(true);
    expect(out.text).toBe("a\nb");
  });

  it("leaves mixed content alone (has both real LF and literal \\n)", () => {
    // If the text already has real newlines, the literal \n is presumed
    // intentional (e.g. discussing escape sequences in docs).
    const input = "real\nbreak with literal\\nshown";
    const out = normalizeEscapedWhitespace(input);
    expect(out.changed).toBe(false);
    expect(out.text).toBe(input);
  });

  it("leaves plain text alone", () => {
    const out = normalizeEscapedWhitespace("just a plain sentence.");
    expect(out.changed).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// combineEnvelopes — burst batching merges N inbounds into one envelope
// ---------------------------------------------------------------------------

function makeItem(messageId: string, text: string): InboundItem {
  const envelope: InboundEnvelope = {
    channel: "feishu",
    accountId: "default",
    senderId: "ou_user",
    chatType: "direct",
    chatId: "oc_chat",
    text,
    messageId,
  };
  return { envelope, messageId, receivedAt: Date.now() };
}

describe("combineEnvelopes", () => {
  it("passes single-item batches through untouched", () => {
    const item = makeItem("om_1", "hello");
    const combined = combineEnvelopes([item]);
    expect(combined).toBe(item.envelope);
  });

  it("merges burst into chronological numbered list using the latest as base", () => {
    const items = [
      makeItem("om_1", "first"),
      makeItem("om_2", "second"),
      makeItem("om_3", "third"),
    ];
    const combined = combineEnvelopes(items);
    // Base fields come from the latest envelope.
    expect(combined.messageId).toBe("om_3");
    expect(combined.text).toBe(
      "[消息 1/3] first\n[消息 2/3] second\n[消息 3/3] third",
    );
    // Metadata for downstream consumers (history enrichment, etc.).
    expect((combined as any).batchSize).toBe(3);
    expect((combined as any).batchMessageIds).toEqual(["om_1", "om_2", "om_3"]);
  });

  it("tolerates items with empty text", () => {
    const items = [makeItem("om_1", ""), makeItem("om_2", "real")];
    const combined = combineEnvelopes(items);
    expect(combined.text).toBe("[消息 1/2] \n[消息 2/2] real");
  });
});

// ---------------------------------------------------------------------------
// Inflight queue — FIFO ordering matches eli per-session turn serialization
// (codex review P1 regression: was a single slot, overwrites stranded the
// first batch's reactions and quote target).
// ---------------------------------------------------------------------------

describe("inflight FIFO", () => {
  it("returns undefined when no batch is queued", () => {
    expect(__takeInflightForTest("oc_empty")).toBeUndefined();
    expect(__inflightDepth("oc_empty")).toBe(0);
  });

  it("preserves multiple batches and pops in FIFO order", () => {
    const batchA = [makeItem("om_A", "first")];
    const batchB = [makeItem("om_B", "second")];
    const batchC = [makeItem("om_C", "third")];
    __seedInflight("oc_x", batchA);
    __seedInflight("oc_x", batchB);
    __seedInflight("oc_x", batchC);
    expect(__inflightDepth("oc_x")).toBe(3);

    const first = __takeInflightForTest("oc_x");
    expect(first?.items[0].messageId).toBe("om_A");
    expect(__inflightDepth("oc_x")).toBe(2);

    const second = __takeInflightForTest("oc_x");
    expect(second?.items[0].messageId).toBe("om_B");

    const third = __takeInflightForTest("oc_x");
    expect(third?.items[0].messageId).toBe("om_C");

    expect(__inflightDepth("oc_x")).toBe(0);
    expect(__takeInflightForTest("oc_x")).toBeUndefined();
  });

  it("scopes inflight queues per chat — different chats don't interleave", () => {
    __seedInflight("oc_a", [makeItem("om_a1", "a1")]);
    __seedInflight("oc_b", [makeItem("om_b1", "b1")]);
    expect(__inflightDepth("oc_a")).toBe(1);
    expect(__inflightDepth("oc_b")).toBe(1);

    const popA = __takeInflightForTest("oc_a");
    expect(popA?.items[0].messageId).toBe("om_a1");
    expect(__inflightDepth("oc_b")).toBe(1); // unaffected
  });

  it("chunkText returns single chunk when under cap", () => {
    const text = "hello";
    expect(chunkText(text)).toEqual(["hello"]);
  });

  it("chunkText splits at paragraph boundaries when possible", () => {
    // Build a deterministic 3-paragraph string just under 3× the test cap.
    const cap = 100;
    const para1 = "a".repeat(80) + "x";
    const para2 = "b".repeat(80) + "y";
    const para3 = "c".repeat(40);
    const text = `${para1}\n\n${para2}\n\n${para3}`;
    const chunks = chunkText(text, cap);
    expect(chunks.length).toBeGreaterThanOrEqual(2);
    // No chunk should exceed the cap.
    for (const c of chunks) expect(c.length).toBeLessThanOrEqual(cap);
    // Concatenating chunks recovers the original semantically (modulo
    // collapsed whitespace at chunk boundaries).
    const recovered = chunks.join("\n\n");
    expect(recovered.replace(/\s+/g, "")).toBe(text.replace(/\s+/g, ""));
  });

  it("chunkText hard-cuts when no boundary is reachable in the upper half", () => {
    // 200 ASCII chars (200 bytes), no spaces/newlines, cap 50 bytes.
    const text = "x".repeat(200);
    const chunks = chunkText(text, 50);
    expect(chunks).toHaveLength(4);
    for (const c of chunks) expect(c.length).toBe(50);
  });

  it("chunkText respects UTF-8 byte limit, not char count (CJK)", () => {
    // 100 CJK chars × 3 bytes/char = 300 bytes; cap 120 bytes → at most
    // ~40 chars per chunk. The bug we're guarding against was capping
    // by chars (=100) and producing one chunk that exceeded the byte cap.
    const text = "中".repeat(100);
    const chunks = chunkText(text, 120);
    expect(chunks.length).toBeGreaterThan(1);
    for (const c of chunks) {
      expect(Buffer.byteLength(c, "utf8")).toBeLessThanOrEqual(120);
    }
    // Concatenation recovers the original (chunks have no separators
    // injected because there were no paragraph/newline breaks).
    expect(chunks.join("")).toBe(text);
  });

  it("chunkText closes + reopens fenced code blocks across chunk boundaries", () => {
    // Build a long fenced block forced to split.
    const fence = "```python\n" + ("print('x')\n".repeat(20)) + "```";
    const chunks = chunkText(fence, 80);
    expect(chunks.length).toBeGreaterThan(1);
    // Every non-final chunk must end with a closing ``` and every
    // non-first chunk must reopen with ```language so the user's
    // markdown renderer keeps the code style intact.
    for (let i = 0; i < chunks.length - 1; i++) {
      expect(chunks[i].trimEnd().endsWith("```")).toBe(true);
    }
    for (let i = 1; i < chunks.length; i++) {
      expect(chunks[i].startsWith("```python")).toBe(true);
    }
  });

  it("chunkText production cap is large enough for most messages", () => {
    // Sanity: cap should keep us safely under Feishu's ~30k content limit.
    expect(MAX_CHUNK_CHARS).toBeGreaterThan(10_000);
    expect(MAX_CHUNK_CHARS).toBeLessThan(30_000);
  });
});

// ---------------------------------------------------------------------------
// stripMentions — clean inbound @-noise so the LLM prompt isn't polluted
// ---------------------------------------------------------------------------

describe("stripMentions", () => {
  it("leaves text without mentions unchanged", () => {
    expect(stripMentions("你好，能帮我查个东西吗")).toBe("你好，能帮我查个东西吗");
  });

  it("strips a single leading @name token", () => {
    expect(stripMentions("@小助手 你好")).toBe("你好");
  });

  it("strips multiple stacked leading mentions", () => {
    expect(stripMentions("@小助手 @secondary 帮我看个东西")).toBe("帮我看个东西");
  });

  it("keeps inline @ references inside the body untouched", () => {
    // We only peel from the front; talking ABOUT @username is fair text.
    expect(stripMentions("能找一下 @张三 提到的那个文档吗")).toBe(
      "能找一下 @张三 提到的那个文档吗",
    );
  });

  it("strips raw <at> wrappers if lark-cli didn't pre-render", () => {
    const raw = '<at user_id="ou_xxx" user_name="bot">@bot</at> 你好';
    expect(stripMentions(raw)).toBe("你好");
  });

  it("handles self-closing <at/> tags", () => {
    expect(stripMentions('<at user_id="ou_xxx"/>你好')).toBe("你好");
  });
});

// ---------------------------------------------------------------------------
// isAppSender — anti-loop filter
// ---------------------------------------------------------------------------

describe("isAppSender", () => {
  it("treats cli_* and app_* as apps (filtered)", () => {
    expect(isAppSender("cli_a9f074df7179dbd2")).toBe(true);
    expect(isAppSender("app_xyz")).toBe(true);
  });

  it("treats ou_* (open_id) as user (kept)", () => {
    expect(isAppSender("ou_98599808ce3ee3c07ab6232848899942")).toBe(false);
  });

  it("treats on_* (union_id) as user (kept)", () => {
    expect(isAppSender("on_unionid12345")).toBe(false);
  });

  it("treats tenant user_id (no prefix) as user (kept)", () => {
    expect(isAppSender("abc123")).toBe(false);
    expect(isAppSender("ckl")).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// friendlyizeError — soften raw eli error envelopes for end users
// ---------------------------------------------------------------------------

describe("friendlyizeError", () => {
  it("passes through non-error text unchanged", () => {
    const ok = "这是 LLM 的正常回复内容。";
    expect(friendlyizeError(ok)).toBe(ok);
  });

  it("does not match text that merely mentions errors casually", () => {
    const casual = "刚才那个 [Error: ...] 是怎么回事";
    expect(friendlyizeError(casual)).toBe(casual);
  });

  it("translates rate-limit errors", () => {
    const raw =
      "[Error: run_model failed in plugin 'builtin': [temporary] openai:gpt-5.5: HTTP 429 Too Many Requests]";
    expect(friendlyizeError(raw)).toContain("限流");
  });

  it("translates context-overflow errors", () => {
    const raw =
      "[Error: run_model failed in plugin 'builtin': context window overflow at 256000 tokens]";
    expect(friendlyizeError(raw)).toContain("上下文");
  });

  it("translates timeout errors", () => {
    const raw =
      "[Error: run_model failed in plugin 'builtin': request timed out after 120s]";
    expect(friendlyizeError(raw)).toContain("超时");
  });

  it("falls back to a generic friendly message for unknown errors", () => {
    const raw =
      "[Error: run_model failed in plugin 'builtin': mysterious_failure_xyz]";
    const out = friendlyizeError(raw);
    expect(out).not.toContain("[Error:");
    expect(out.length).toBeLessThan(60);
  });
});

describe("onTurnEnd lifecycle hook", () => {
  it("pops one head batch from the FIFO and is a no-op when empty", async () => {
    // Import lazily so the test stays inside this describe's scope.
    const { feishuCliPlugin } = await import("../plugins/feishu-cli.ts");
    const onTurnEnd = feishuCliPlugin.lifecycle?.onTurnEnd;
    if (!onTurnEnd) throw new Error("onTurnEnd not wired");

    __seedInflight("oc_q", [makeItem("om_1", "first")]);
    __seedInflight("oc_q", [makeItem("om_2", "second")]);
    expect(__inflightDepth("oc_q")).toBe(2);

    await onTurnEnd({ chatId: "oc_q", accountId: "default", sessionId: "feishu:default:oc_q" });
    expect(__inflightDepth("oc_q")).toBe(1);

    await onTurnEnd({ chatId: "oc_q", accountId: "default", sessionId: "feishu:default:oc_q" });
    expect(__inflightDepth("oc_q")).toBe(0);

    // Empty chat: must not throw, must remain empty.
    await onTurnEnd({ chatId: "oc_q", accountId: "default", sessionId: "feishu:default:oc_q" });
    expect(__inflightDepth("oc_q")).toBe(0);
  });
});
