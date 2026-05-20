import { afterEach, describe, expect, it } from "bun:test";
import {
  __inflightDepth,
  __resetChannelState,
  __resetSeenEventIds,
  __seedInflight,
  __takeInflightForTest,
  alreadySeen,
  combineEnvelopes,
  normalizeEscapedWhitespace,
  routeArgs,
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
});
