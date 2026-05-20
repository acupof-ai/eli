/**
 * Integration test: full inbound/outbound bridge cycle.
 *
 * Spins up a mock eli server + the sidecar outbound server + a mock channel
 * plugin, then verifies end-to-end message flow in both directions.
 */

import { describe, it, expect, beforeAll, afterAll, beforeEach } from "bun:test";
import express from "express";
import { readFileSync } from "node:fs";
import type { Server } from "node:http";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { registry } from "../src/registry.js";
import { initBridge, sendToEli, startOutboundServer } from "../src/bridge.js";
import { ELI_BRIDGE_CONTRACT_VERSION } from "../src/contract.js";
import { envelopeToEliMessage, parseOutboundTarget } from "../src/envelope.js";
import { beginPendingTyping, pendingTyping, sessionContexts } from "../src/runtime.js";
import type { ChannelPlugin, InboundEnvelope, EliChannelMessage } from "../src/types.js";

const MOCK_ELI_PORT = 13100;
const SIDECAR_PORT = 13101;

let mockEliServer: Server;
let sidecarServer: Server | undefined;
let capturedInbound: any[] = [];
let sentMessages: Array<{ text: string; chatId: string; accountId: string; kind?: string }> = [];
let cleanupCalls: Array<{ typingState: any; accountId: string }> = [];
let turnEndCalls: Array<{ chatId: string; accountId: string; sessionId: string }> = [];

function fixture(name: string): any {
  const dir = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../contracts/v1");
  return JSON.parse(readFileSync(path.join(dir, name), "utf8"));
}

function contractMessage(
  message: Omit<EliChannelMessage, "contract_version">,
): EliChannelMessage {
  return { contract_version: ELI_BRIDGE_CONTRACT_VERSION, ...message };
}

function contractPayload<T extends Record<string, unknown>>(
  payload: T,
): T & { contract_version: string } {
  return { contract_version: ELI_BRIDGE_CONTRACT_VERSION, ...payload };
}

function connectionRefusedError(): Error {
  const cause = Object.assign(new Error("connect ECONNREFUSED 127.0.0.1:3100"), {
    code: "ECONNREFUSED",
  });
  return Object.assign(new TypeError("fetch failed"), { cause });
}

// Mock channel plugin with outbound.sendText
const mockChannel: ChannelPlugin = {
  meta: { id: "mock-channel", label: "Mock Channel" },
  config: {
    listAccountIds: () => ["default"],
    resolveAccount: () => ({}),
  },
  capabilities: { chatTypes: ["direct"] },
  outbound: {
    deliveryMode: "direct",
    sendText: async ({ text, to, accountId, kind }: any) => {
      const entry: any = { text, chatId: to, accountId };
      if (kind !== undefined) entry.kind = kind;
      sentMessages.push(entry);
      return { ok: true };
    },
  },
  lifecycle: {
    onOutboundReply: async ({ typingState, accountId }: any) => {
      cleanupCalls.push({ typingState, accountId });
    },
    onTurnEnd: async ({ chatId, accountId, sessionId }: any) => {
      turnEndCalls.push({ chatId, accountId, sessionId });
    },
  },
};

beforeAll(async () => {
  capturedInbound = [];
  sentMessages = [];
  cleanupCalls = [];

  // Mock eli server — captures inbound POSTs.
  await new Promise<void>((resolve) => {
    const app = express();
    app.use(express.json());
    app.post("/inbound", (req, res) => {
      capturedInbound.push(req.body);
      res.status(200).json({ ok: true });
    });
    mockEliServer = app.listen(MOCK_ELI_PORT, () => resolve());
  });

  initBridge({
    eli_url: `http://127.0.0.1:${MOCK_ELI_PORT}`,
    port: SIDECAR_PORT,
    plugins: [],
    channels: {},
  });

  registry.channels.clear();
  registry.registerChannel(mockChannel);

  sidecarServer = await startOutboundServer(SIDECAR_PORT);
});

beforeEach(() => {
  capturedInbound = [];
  sentMessages = [];
  cleanupCalls = [];
  turnEndCalls = [];
  pendingTyping.clear();
  sessionContexts.clear();
  registry.tools.clear();
  mockChannel.lifecycle = {
    onOutboundReply: async ({ typingState, accountId }: any) => {
      cleanupCalls.push({ typingState, accountId });
    },
    onTurnEnd: async ({ chatId, accountId, sessionId }: any) => {
      turnEndCalls.push({ chatId, accountId, sessionId });
    },
  };
  registry.channels.clear();
  registry.registerChannel(mockChannel);
});

afterAll(() => {
  mockEliServer?.close();
  sidecarServer?.close();
});

// ---------------------------------------------------------------------------
// Inbound: sidecar → eli
// ---------------------------------------------------------------------------

describe("inbound: sendToEli", () => {
  it("POSTs normalized ChannelMessage to eli /inbound", async () => {
    capturedInbound = [];

    await sendToEli({
      channel: "mock-channel",
      accountId: "default",
      senderId: "sender_1",
      senderName: "Bob",
      chatType: "direct",
      text: "Hello from mock",
    });

    expect(capturedInbound).toHaveLength(1);
    const msg = capturedInbound[0];
    expect(msg.session_id).toBe("mock-channel:default:sender_1");
    expect(msg.contract_version).toBe(ELI_BRIDGE_CONTRACT_VERSION);
    expect(msg.channel).toBe("webhook");
    expect(msg.content).toBe("Hello from mock");
    expect(msg.chat_id).toBe("sender_1");
    expect(msg.is_active).toBe(true);
    expect(msg.context.source_channel).toBe("mock-channel");
    expect(msg.context.sender_name).toBe("Bob");
  });

  it("retries ECONNREFUSED up to 3 times before succeeding", async () => {
    const originalFetch = globalThis.fetch;
    let attempts = 0;

    globalThis.fetch = (async () => {
      attempts += 1;
      if (attempts < 4) throw connectionRefusedError();
      return new Response(JSON.stringify({ ok: true }), { status: 200 });
    }) as typeof fetch;

    try {
      await sendToEli({
        channel: "mock-channel",
        accountId: "default",
        senderId: "sender_retry",
        chatType: "direct",
        text: "retry me",
      });
    } finally {
      globalThis.fetch = originalFetch;
    }

    expect(attempts).toBe(4);
  });
});

// ---------------------------------------------------------------------------
// Outbound: eli → sidecar → channel plugin
// ---------------------------------------------------------------------------

describe("outbound: eli callback", () => {
  it("routes response to the correct channel plugin's sendText", async () => {
    const resp = await fetch(`http://127.0.0.1:${SIDECAR_PORT}/outbound`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(contractMessage({
        session_id: "mock-channel:default:user_1",
        channel: "webhook",
        content: "Hello from eli",
        chat_id: "user_1",
        context: {
          source_channel: "mock-channel",
          account_id: "default",
          chat_type: "direct",
          sender_id: "user_1",
        },
        output_channel: "webhook",
      })),
    });

    expect(resp.status).toBe(200);
    expect(sentMessages).toHaveLength(1);
    expect(sentMessages[0].text).toBe("Hello from eli");
    expect(sentMessages[0].chatId).toBe("user_1");
    expect(sentMessages[0].accountId).toBe("default");
  });

  it("runs cleanup-only outbound without sending text", async () => {
    pendingTyping.set("mock-channel:default:user_cleanup", {
      typingState: { reaction: "thinking" },
      cfg: { channels: {} },
      accountId: "default",
    });

    const resp = await fetch(`http://127.0.0.1:${SIDECAR_PORT}/outbound`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(contractMessage({
        session_id: "mock-channel:default:user_cleanup",
        channel: "webhook",
        content: "",
        chat_id: "user_cleanup",
        context: {
          source_channel: "mock-channel",
          account_id: "default",
          chat_type: "direct",
          _eli_cleanup_only: true,
        },
        output_channel: "webhook",
      })),
    });

    const body = (await resp.json()) as any;
    expect(resp.status).toBe(200);
    expect(body.ok).toBe(true);
    expect(body.cleanup_only).toBe(true);
    expect(sentMessages).toHaveLength(0);
    expect(cleanupCalls).toHaveLength(1);
    expect(cleanupCalls[0].typingState).toEqual({ reaction: "thinking" });
    expect(cleanupCalls[0].accountId).toBe("default");
    expect(pendingTyping.has("mock-channel:default:user_cleanup")).toBe(false);
  });

  it("cleanup-only outbound invokes channel.lifecycle.onTurnEnd", async () => {
    // Cleanup-only means the LLM produced no final text (e.g. empty
    // render_outbound). The plugin's onTurnEnd is the only signal it
    // gets to drop per-turn state (feishu-cli's inflight FIFO + its
    // matching Typing reactions). Without this hook, the next real
    // turn would pop a stale batch.
    const resp = await fetch(`http://127.0.0.1:${SIDECAR_PORT}/outbound`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(contractMessage({
        session_id: "mock-channel:default:user_endturn",
        channel: "webhook",
        content: "",
        chat_id: "user_endturn",
        context: {
          source_channel: "mock-channel",
          account_id: "default",
          chat_type: "direct",
          _eli_cleanup_only: true,
        },
        output_channel: "webhook",
      })),
    });

    expect(resp.status).toBe(200);
    expect(sentMessages).toHaveLength(0);
    expect(turnEndCalls).toHaveLength(1);
    expect(turnEndCalls[0].chatId).toBe("user_endturn");
    expect(turnEndCalls[0].accountId).toBe("default");
    expect(turnEndCalls[0].sessionId).toBe("mock-channel:default:user_endturn");
  });

  it("final-reply outbound does NOT invoke onTurnEnd (sendText pops state)", async () => {
    // Symmetric guarantee: the regular reply path already pops the
    // inflight batch through sendText/takeInflight in the plugin.
    // Double-firing onTurnEnd here would pop a SECOND batch and
    // misalign future replies — codex flagged this exact double-pop
    // hazard in review #4.
    const resp = await fetch(`http://127.0.0.1:${SIDECAR_PORT}/outbound`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(contractMessage({
        session_id: "mock-channel:default:user_final",
        channel: "webhook",
        content: "here is the answer",
        chat_id: "user_final",
        context: {
          source_channel: "mock-channel",
          account_id: "default",
          chat_type: "direct",
        },
        output_channel: "webhook",
      })),
    });

    expect(resp.status).toBe(200);
    expect(sentMessages).toHaveLength(1);
    expect(sentMessages[0].text).toBe("here is the answer");
    // Default kind: undefined treated as "final" by the bridge.
    expect(sentMessages[0].kind).toBe("final");
    expect(turnEndCalls).toHaveLength(0);
  });

  it("mid-turn outbound forwards kind=notice and does NOT call onTurnEnd", async () => {
    // message.send and new-session greetings set context._eli_mid_turn.
    // The plugin sees kind:"notice" so it keeps per-turn state alive;
    // onTurnEnd is reserved for end-of-turn cleanup-only paths and must
    // not fire for mid-turn dispatches.
    const resp = await fetch(`http://127.0.0.1:${SIDECAR_PORT}/outbound`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(contractMessage({
        session_id: "mock-channel:default:user_midturn",
        channel: "webhook",
        content: "working on it…",
        chat_id: "user_midturn",
        context: {
          source_channel: "mock-channel",
          account_id: "default",
          chat_type: "direct",
          _eli_mid_turn: true,
        },
        output_channel: "webhook",
      })),
    });

    expect(resp.status).toBe(200);
    expect(sentMessages).toHaveLength(1);
    expect(sentMessages[0].kind).toBe("notice");
    expect(turnEndCalls).toHaveLength(0);
    // Typing state must survive mid-turn notices.
    expect(cleanupCalls).toHaveLength(0);
  });

  it("rejects unsupported contract_version", async () => {
    const resp = await fetch(`http://127.0.0.1:${SIDECAR_PORT}/outbound`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        ...fixture("channel-message.json"),
        contract_version: "eli.sidecar.v9",
      }),
    });

    expect(resp.status).toBe(400);
    const body = (await resp.json()) as any;
    expect(body.error).toContain("unsupported contract_version");
  });

  it("queues typing cleanup when outbound arrives before typing setup completes", async () => {
    let resolveTypingState: ((value: any) => void) | undefined;

    mockChannel.lifecycle = {
      onInboundMessage: async () => {
        return await new Promise((resolve) => {
          resolveTypingState = resolve;
        });
      },
      onOutboundReply: async ({ typingState, accountId }: any) => {
        cleanupCalls.push({ typingState, accountId });
      },
    };
    registry.channels.clear();
    registry.registerChannel(mockChannel);

    void beginPendingTyping({
      channelPlugin: mockChannel,
      cfg: { channels: {} },
      messageId: "msg_race",
      accountId: "default",
      sessionId: "mock-channel:default:user_race",
    });

    const resp = await fetch(`http://127.0.0.1:${SIDECAR_PORT}/outbound`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(contractMessage({
        session_id: "mock-channel:default:user_race",
        channel: "webhook",
        content: "Hello from eli",
        chat_id: "user_race",
        context: {
          source_channel: "mock-channel",
          account_id: "default",
          chat_type: "direct",
          sender_id: "user_race",
        },
        output_channel: "webhook",
      })),
    });

    expect(resp.status).toBe(200);
    expect(sentMessages).toHaveLength(1);
    expect(cleanupCalls).toHaveLength(0);
    expect(pendingTyping.has("mock-channel:default:user_race")).toBe(false);

    resolveTypingState?.({ reaction: "thinking" });
    await new Promise((resolve) => setTimeout(resolve, 10));

    expect(cleanupCalls).toHaveLength(1);
    expect(cleanupCalls[0].typingState).toEqual({ reaction: "thinking" });
    expect(cleanupCalls[0].accountId).toBe("default");
    expect(pendingTyping.has("mock-channel:default:user_race")).toBe(false);
  });

  it("cleans up typing via legacy path when channel has no lifecycle hooks", async () => {
    // Simulate the real feishu plugin: no lifecycle.onOutboundReply.
    // The sidecar must use the legacy removeTypingState path.
    const noLifecycleChannel: ChannelPlugin = {
      meta: { id: "no-lifecycle", label: "No Lifecycle" },
      config: { listAccountIds: () => ["default"], resolveAccount: () => ({}) },
      capabilities: { chatTypes: ["direct"] },
      outbound: {
        deliveryMode: "direct",
        sendText: async ({ text, to, accountId }: any) => {
          sentMessages.push({ text, chatId: to, accountId });
          return { ok: true };
        },
      },
      // No lifecycle hooks — matches the real feishu channel plugin.
    };
    registry.channels.clear();
    registry.registerChannel(noLifecycleChannel);

    pendingTyping.set("no-lifecycle:default:user_legacy", {
      typingState: { messageId: "om_test_123", reactionId: "rxn_456" },
      cfg: { channels: {} },
      accountId: "default",
    });

    const resp = await fetch(`http://127.0.0.1:${SIDECAR_PORT}/outbound`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(contractMessage({
        session_id: "no-lifecycle:default:user_legacy",
        channel: "webhook",
        content: "reply",
        chat_id: "user_legacy",
        context: {
          source_channel: "no-lifecycle",
          account_id: "default",
        },
        output_channel: "webhook",
      })),
    });

    expect(resp.status).toBe(200);
    expect(sentMessages).toHaveLength(1);
    // Typing state must be removed even without lifecycle hooks.
    await new Promise((r) => setTimeout(r, 50));
    expect(pendingTyping.has("no-lifecycle:default:user_legacy")).toBe(false);
  });

  it("cleans up typing via session_id fallback when context lacks source_channel", async () => {
    // Regression: before the context propagation fix, normal outbounds
    // had no source_channel in context. The sidecar falls back to parsing
    // session_id. Verify typing cleanup still works in this fallback path.
    const noLifecycleChannel: ChannelPlugin = {
      meta: { id: "no-lifecycle", label: "No Lifecycle" },
      config: { listAccountIds: () => ["default"], resolveAccount: () => ({}) },
      capabilities: { chatTypes: ["direct"] },
      outbound: {
        deliveryMode: "direct",
        sendText: async ({ text, to, accountId }: any) => {
          sentMessages.push({ text, chatId: to, accountId });
          return { ok: true };
        },
      },
    };
    registry.channels.clear();
    registry.registerChannel(noLifecycleChannel);

    pendingTyping.set("no-lifecycle:default:user_fallback", {
      typingState: { messageId: "om_test_789", reactionId: "rxn_abc" },
      cfg: { channels: {} },
      accountId: "default",
    });

    // Send outbound WITHOUT source_channel — only session_id for routing.
    const resp = await fetch(`http://127.0.0.1:${SIDECAR_PORT}/outbound`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(contractMessage({
        session_id: "no-lifecycle:default:user_fallback",
        channel: "webhook",
        content: "reply",
        chat_id: "user_fallback",
        context: {
          channel: "$webhook",
          chat_id: "user_fallback",
        },
        output_channel: "webhook",
      })),
    });

    expect(resp.status).toBe(200);
    expect(sentMessages).toHaveLength(1);
    await new Promise((r) => setTimeout(r, 50));
    expect(pendingTyping.has("no-lifecycle:default:user_fallback")).toBe(false);
  });

  it("returns 404 for unknown source_channel", async () => {
    const resp = await fetch(`http://127.0.0.1:${SIDECAR_PORT}/outbound`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(contractMessage({
        session_id: "x",
        channel: "webhook",
        content: "Hello",
        chat_id: "u",
        context: { source_channel: "nonexistent" },
        output_channel: "webhook",
      })),
    });
    expect(resp.status).toBe(404);
  });

  it("returns 400 when source_channel missing from context", async () => {
    const resp = await fetch(`http://127.0.0.1:${SIDECAR_PORT}/outbound`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(contractMessage({
        session_id: "x",
        channel: "webhook",
        content: "Hello",
        chat_id: "u",
        context: {},
        output_channel: "webhook",
      })),
    });
    expect(resp.status).toBe(400);
  });
});

// ---------------------------------------------------------------------------
// Tool execution notices
// ---------------------------------------------------------------------------

describe("tool execution", () => {
  it("sends top-level description back to the active channel before executing", async () => {
    registry.registerTool({
      name: "bash",
      description: "Run shell",
      parameters: {},
      execute: async () => ({ content: [{ type: "text", text: "ok" }] }),
    });

    sessionContexts.set("mock-channel:default:user_1", {
      channel: "mock-channel",
      messageId: "msg_1",
      chatId: "user_1",
      channelTarget: "route:user_1",
      accountId: "default",
      senderId: "user_1",
      chatType: "direct",
      cfg: {},
    });

    const resp = await fetch(`http://127.0.0.1:${SIDECAR_PORT}/tools/bash`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(contractPayload({
        session_id: "mock-channel:default:user_1",
        description: "查看当前工作目录",
        params: {
          cmd: "pwd",
        },
      })),
    });

    expect(resp.status).toBe(200);
    expect(sentMessages).toHaveLength(1);
    expect(sentMessages[0]).toEqual({
      text: "查看当前工作目录",
      chatId: "route:user_1",
      accountId: "default",
      kind: "notice",
    });
  });

  it("lets channels override tool notice rendering", async () => {
    mockChannel.lifecycle = {
      onOutboundReply: async ({ typingState, accountId }: any) => {
        cleanupCalls.push({ typingState, accountId });
      },
      renderToolCallText: async (event) =>
        event.phase === "after" ? `完成 ${event.toolName}` : null,
    };
    registry.channels.clear();
    registry.registerChannel(mockChannel);

    registry.registerTool({
      name: "custom_tool",
      description: "Custom tool",
      parameters: {},
      execute: async () => ({ content: [{ type: "text", text: "done" }] }),
    });

    sessionContexts.set("mock-channel:default:user_2", {
      channel: "mock-channel",
      messageId: "msg_2",
      chatId: "user_2",
      accountId: "default",
      senderId: "user_2",
      chatType: "direct",
      cfg: {},
    });

    const resp = await fetch(`http://127.0.0.1:${SIDECAR_PORT}/tools/custom_tool`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(contractPayload({
        session_id: "mock-channel:default:user_2",
        params: { value: 1 },
      })),
    });

    expect(resp.status).toBe(200);
    expect(sentMessages).toHaveLength(1);
    expect(sentMessages[0]).toEqual({
      text: "完成 custom_tool",
      chatId: "user_2",
      accountId: "default",
      kind: "notice",
    });
  });

  it("sends direct notices for builtin tools without clearing typing state", async () => {
    pendingTyping.set("mock-channel:default:user_3", {
      typingState: { reaction: "thinking" },
      cfg: { channels: {} },
      accountId: "default",
    });
    sessionContexts.set("mock-channel:default:user_3", {
      channel: "mock-channel",
      messageId: "msg_3",
      chatId: "user_3",
      channelTarget: "route:user_3",
      accountId: "default",
      senderId: "user_3",
      chatType: "direct",
      cfg: {},
    });

    const resp = await fetch(`http://127.0.0.1:${SIDECAR_PORT}/notify`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(contractPayload({
        session_id: "mock-channel:default:user_3",
        text: "正在读取文件",
      })),
    });

    const body = (await resp.json()) as any;
    expect(resp.status).toBe(200);
    expect(body).toEqual({ ok: true, delivered: true });
    expect(sentMessages).toHaveLength(1);
    expect(sentMessages[0]).toEqual({
      text: "正在读取文件",
      chatId: "route:user_3",
      accountId: "default",
      kind: "notice",
    });
    expect(cleanupCalls).toHaveLength(0);
    expect(pendingTyping.has("mock-channel:default:user_3")).toBe(true);
  });
});

// ---------------------------------------------------------------------------
// Outbound media pipeline
// ---------------------------------------------------------------------------

describe("outbound media", () => {
  it("calls sendMedia for each item in context.outbound_media", async () => {
    let mediaCalls: Array<{ mediaPath: string; mediaType: string }> = [];
    const mediaChannel: ChannelPlugin = {
      meta: { id: "media-channel", label: "Media Channel" },
      config: { listAccountIds: () => ["default"], resolveAccount: () => ({}) },
      capabilities: { chatTypes: ["direct"] },
      outbound: {
        deliveryMode: "direct",
        sendText: async ({ text, to, accountId }: any) => {
          sentMessages.push({ text, chatId: to, accountId });
          return { ok: true };
        },
        sendMedia: async (params: any) => {
          mediaCalls.push({ mediaPath: params.mediaPath, mediaType: params.mediaType });
          return { ok: true };
        },
      },
    };
    registry.channels.clear();
    registry.registerChannel(mediaChannel);

    const resp = await fetch(`http://127.0.0.1:${SIDECAR_PORT}/outbound`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(contractMessage({
        session_id: "media-channel:default:user_m",
        channel: "webhook",
        content: "Here is the image",
        chat_id: "user_m",
        context: {
          source_channel: "media-channel",
          account_id: "default",
          outbound_media: [
            { path: "/tmp/test.png", media_type: "image", mime_type: "image/png" },
          ],
        },
        output_channel: "webhook",
      })),
    });

    expect(resp.status).toBe(200);
    // Text must be sent.
    expect(sentMessages).toHaveLength(1);
    expect(sentMessages[0].text).toBe("Here is the image");

    // Give fire-and-forget media a moment to dispatch.
    await new Promise((r) => setTimeout(r, 50));
    expect(mediaCalls).toHaveLength(1);
    expect(mediaCalls[0].mediaPath).toBe("/tmp/test.png");
    expect(mediaCalls[0].mediaType).toBe("image");
  });

  it("prefers top-level media when contract payload includes it", async () => {
    let mediaCalls: Array<{ mediaPath: string; mediaType: string }> = [];
    const mediaChannel: ChannelPlugin = {
      meta: { id: "mock-channel", label: "Media Channel" },
      config: { listAccountIds: () => ["default"], resolveAccount: () => ({}) },
      capabilities: { chatTypes: ["direct"] },
      outbound: {
        deliveryMode: "direct",
        sendText: async ({ text, to, accountId }: any) => {
          sentMessages.push({ text, chatId: to, accountId });
          return { ok: true };
        },
        sendMedia: async (params: any) => {
          mediaCalls.push({ mediaPath: params.mediaPath, mediaType: params.mediaType });
          return { ok: true };
        },
      },
    };
    registry.channels.clear();
    registry.registerChannel(mediaChannel);

    const resp = await fetch(`http://127.0.0.1:${SIDECAR_PORT}/outbound`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(fixture("channel-message.json")),
    });

    expect(resp.status).toBe(200);
    await new Promise((r) => setTimeout(r, 50));
    expect(mediaCalls).toEqual([{ mediaPath: "/tmp/fixture.png", mediaType: "image" }]);
  });

  it("sends text even when no outbound_media present (backward compatible)", async () => {
    const resp = await fetch(`http://127.0.0.1:${SIDECAR_PORT}/outbound`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(contractMessage({
        session_id: "mock-channel:default:user_compat",
        channel: "webhook",
        content: "Normal text reply",
        chat_id: "user_compat",
        context: {
          source_channel: "mock-channel",
          account_id: "default",
        },
        output_channel: "webhook",
      })),
    });

    expect(resp.status).toBe(200);
    expect(sentMessages).toHaveLength(1);
    expect(sentMessages[0].text).toBe("Normal text reply");
  });

  it("sendText errors still return 500 (not swallowed)", async () => {
    const failChannel: ChannelPlugin = {
      meta: { id: "fail-channel", label: "Fail Channel" },
      config: { listAccountIds: () => ["default"], resolveAccount: () => ({}) },
      capabilities: { chatTypes: ["direct"] },
      outbound: {
        deliveryMode: "direct",
        sendText: async () => { throw new Error("sendText boom"); },
      },
    };
    registry.channels.clear();
    registry.registerChannel(failChannel);

    const resp = await fetch(`http://127.0.0.1:${SIDECAR_PORT}/outbound`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(contractMessage({
        session_id: "fail-channel:default:user_err",
        channel: "webhook",
        content: "Will fail",
        chat_id: "user_err",
        context: {
          source_channel: "fail-channel",
          account_id: "default",
        },
        output_channel: "webhook",
      })),
    });

    expect(resp.status).toBe(500);
    const body = (await resp.json()) as any;
    expect(body.error).toContain("sendText boom");
  });

  it("sendMedia failure does not block text delivery", async () => {
    let mediaCalled = false;
    const mixedChannel: ChannelPlugin = {
      meta: { id: "mixed-channel", label: "Mixed Channel" },
      config: { listAccountIds: () => ["default"], resolveAccount: () => ({}) },
      capabilities: { chatTypes: ["direct"] },
      outbound: {
        deliveryMode: "direct",
        sendText: async ({ text, to, accountId }: any) => {
          sentMessages.push({ text, chatId: to, accountId });
          return { ok: true };
        },
        sendMedia: async () => {
          mediaCalled = true;
          throw new Error("sendMedia boom");
        },
      },
    };
    registry.channels.clear();
    registry.registerChannel(mixedChannel);

    const resp = await fetch(`http://127.0.0.1:${SIDECAR_PORT}/outbound`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(contractMessage({
        session_id: "mixed-channel:default:user_mix",
        channel: "webhook",
        content: "Text should still arrive",
        chat_id: "user_mix",
        context: {
          source_channel: "mixed-channel",
          account_id: "default",
          outbound_media: [
            { path: "/tmp/broken.png", media_type: "image", mime_type: "image/png" },
          ],
        },
        output_channel: "webhook",
      })),
    });

    // Text must succeed with 200.
    expect(resp.status).toBe(200);
    expect(sentMessages).toHaveLength(1);
    expect(sentMessages[0].text).toBe("Text should still arrive");

    // Media was attempted but failed — should not affect response.
    await new Promise((r) => setTimeout(r, 50));
    expect(mediaCalled).toBe(true);
  });

  it("skips sendMedia when channel does not support it", async () => {
    // mock-channel has no sendMedia.
    registry.channels.clear();
    registry.registerChannel(mockChannel);

    const resp = await fetch(`http://127.0.0.1:${SIDECAR_PORT}/outbound`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(contractMessage({
        session_id: "mock-channel:default:user_nosm",
        channel: "webhook",
        content: "Text with media in context",
        chat_id: "user_nosm",
        context: {
          source_channel: "mock-channel",
          account_id: "default",
          outbound_media: [
            { path: "/tmp/ignored.png", media_type: "image", mime_type: "image/png" },
          ],
        },
        output_channel: "webhook",
      })),
    });

    // Must still succeed — media is just ignored.
    expect(resp.status).toBe(200);
    expect(sentMessages).toHaveLength(1);
    expect(sentMessages[0].text).toBe("Text with media in context");
  });

  it("adapts params for feishu channel (mediaUrl instead of mediaPath)", async () => {
    let feishuMediaParams: any = null;
    const feishuChannel: ChannelPlugin = {
      meta: { id: "openclaw-lark", label: "Feishu" },
      config: { listAccountIds: () => ["default"], resolveAccount: () => ({}) },
      capabilities: { chatTypes: ["direct"] },
      outbound: {
        deliveryMode: "direct",
        sendText: async ({ text, to, accountId }: any) => {
          sentMessages.push({ text, chatId: to, accountId });
          return { ok: true };
        },
        sendMedia: async (params: any) => {
          feishuMediaParams = params;
          return { ok: true };
        },
      },
    };
    registry.channels.clear();
    registry.registerChannel(feishuChannel);

    const resp = await fetch(`http://127.0.0.1:${SIDECAR_PORT}/outbound`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(contractMessage({
        session_id: "openclaw-lark:default:user_fs",
        channel: "webhook",
        content: "Feishu reply with image",
        chat_id: "user_fs",
        context: {
          source_channel: "openclaw-lark",
          account_id: "default",
          outbound_media: [
            { path: "/tmp/feishu.png", media_type: "image", mime_type: "image/png" },
          ],
        },
        output_channel: "webhook",
      })),
    });

    expect(resp.status).toBe(200);
    await new Promise((r) => setTimeout(r, 50));
    expect(feishuMediaParams).not.toBeNull();
    // Feishu uses mediaUrl, not mediaPath.
    expect(feishuMediaParams.mediaUrl).toBe("/tmp/feishu.png");
    expect(feishuMediaParams.mediaLocalRoots).toEqual(["/tmp"]);
    expect(feishuMediaParams.mediaPath).toBeUndefined();
  });
});

// ---------------------------------------------------------------------------
// Envelope conversion
// ---------------------------------------------------------------------------

describe("envelope conversion", () => {
  it("maps InboundEnvelope to EliChannelMessage with correct session_id", () => {
    const msg = envelopeToEliMessage({
      channel: "lark",
      accountId: "bot1",
      senderId: "user_456",
      senderName: "Alice",
      chatType: "group",
      chatId: "group_789",
      groupLabel: "Team",
      text: "Hi team",
    });

    expect(msg.session_id).toBe("lark:bot1:group_789");
    expect(msg.contract_version).toBe(ELI_BRIDGE_CONTRACT_VERSION);
    expect(msg.channel).toBe("webhook");
    expect(msg.chat_id).toBe("group_789");
    expect(msg.content).toBe("Hi team");
    expect(msg.context.source_channel).toBe("lark");
    expect(msg.context.chat_type).toBe("group");
    expect(msg.context.group_label).toBe("Team");
  });

  it("parseOutboundTarget extracts routing fields", () => {
    const target = parseOutboundTarget({
      contract_version: ELI_BRIDGE_CONTRACT_VERSION,
      session_id: "lark:default:u1",
      channel: "webhook",
      content: "resp",
      chat_id: "u1",
      is_active: false,
      context: { source_channel: "lark", account_id: "default", chat_type: "direct" },
      output_channel: "webhook",
    });

    expect(target.sourceChannel).toBe("lark");
    expect(target.accountId).toBe("default");
    expect(target.chatId).toBe("u1");
    expect(target.chatType).toBe("direct");
  });

  it("matches the shared inbound fixture", () => {
    const env: InboundEnvelope = {
      channel: "mock-channel",
      accountId: "default",
      senderId: "sender_fixture",
      senderName: "Fixture User",
      chatType: "direct",
      chatId: "user_fixture",
      text: "Hello from fixture",
      media: [
        {
          media_type: "image",
          mime_type: "image/png",
          filename: "fixture.png",
          data_base64: "AQID",
        },
      ],
      channel_target: "route:user_fixture",
    };

    expect(envelopeToEliMessage(env)).toEqual(fixture("inbound-message.json"));
  });
});

// ---------------------------------------------------------------------------
// Health check
// ---------------------------------------------------------------------------

describe("health check", () => {
  it("returns registered channels and tools", async () => {
    const resp = await fetch(`http://127.0.0.1:${SIDECAR_PORT}/health`);
    const body = (await resp.json()) as any;

    expect(resp.status).toBe(200);
    expect(body.status).toBe("ok");
    expect(body.channels).toContain("mock-channel");
  });
});

// ---------------------------------------------------------------------------
// Edge case: malformed session_id routing
// ---------------------------------------------------------------------------

describe("outbound routing edge cases", () => {
  it("returns 400 for session_id with empty parts (colon-only)", async () => {
    const resp = await fetch(`http://127.0.0.1:${SIDECAR_PORT}/outbound`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(contractMessage({
        session_id: ":::",
        channel: "webhook",
        content: "test",
        chat_id: "u",
        context: {},
        output_channel: "webhook",
      })),
    });
    expect(resp.status).toBe(400);
  });

  it("returns 400 for single-segment session_id", async () => {
    const resp = await fetch(`http://127.0.0.1:${SIDECAR_PORT}/outbound`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(contractMessage({
        session_id: "x",
        channel: "webhook",
        content: "test",
        chat_id: "u",
        context: {},
        output_channel: "webhook",
      })),
    });
    expect(resp.status).toBe(400);
  });

  it("returns 400 for empty session_id and no context", async () => {
    const resp = await fetch(`http://127.0.0.1:${SIDECAR_PORT}/outbound`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(contractMessage({
        session_id: "",
        channel: "webhook",
        content: "test",
        chat_id: "u",
        context: {},
        output_channel: "webhook",
      })),
    });
    expect(resp.status).toBe(400);
  });
});

// ---------------------------------------------------------------------------
// Edge case: tool execution
// ---------------------------------------------------------------------------

describe("tool execution edge cases", () => {
  it("returns 404 for unknown tool", async () => {
    const resp = await fetch(`http://127.0.0.1:${SIDECAR_PORT}/tools/nonexistent_tool`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(contractPayload({ params: {} })),
    });
    expect(resp.status).toBe(404);
    const body = (await resp.json()) as any;
    expect(body.error).toContain("nonexistent_tool");
  });

  it("executes tool without session context (no auth wrapping)", async () => {
    registry.registerTool({
      name: "standalone_tool",
      description: "A tool",
      parameters: {},
      execute: async (_id: string, params: any) => ({
        content: [{ type: "text", text: `result: ${JSON.stringify(params)}` }],
      }),
    });

    const resp = await fetch(`http://127.0.0.1:${SIDECAR_PORT}/tools/standalone_tool`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(contractPayload({ params: { key: "value" } })),
    });
    expect(resp.status).toBe(200);
    const body = (await resp.json()) as any;
    expect(body.content[0].text).toContain("value");
    // No session context → no notice sent.
    expect(sentMessages).toHaveLength(0);
  });

  it("returns 500 when tool execution throws", async () => {
    registry.registerTool({
      name: "failing_tool",
      description: "Fails",
      parameters: {},
      execute: async () => { throw new Error("boom"); },
    });

    const resp = await fetch(`http://127.0.0.1:${SIDECAR_PORT}/tools/failing_tool`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(contractPayload({ params: {} })),
    });
    expect(resp.status).toBe(500);
    const body = (await resp.json()) as any;
    expect(body.content[0].text).toContain("boom");
  });
});

// ---------------------------------------------------------------------------
// Edge case: /notify endpoint
// ---------------------------------------------------------------------------

describe("/notify edge cases", () => {
  it("returns 400 when session_id is missing", async () => {
    const resp = await fetch(`http://127.0.0.1:${SIDECAR_PORT}/notify`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ text: "notice" }),
    });
    expect(resp.status).toBe(400);
    const body = (await resp.json()) as any;
    expect(body.error).toContain("session_id");
  });

  it("returns 400 when text is missing", async () => {
    const resp = await fetch(`http://127.0.0.1:${SIDECAR_PORT}/notify`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ session_id: "mock-channel:default:u1" }),
    });
    expect(resp.status).toBe(400);
    const body = (await resp.json()) as any;
    expect(body.error).toContain("text");
  });

  it("returns delivered:false for expired/unknown session", async () => {
    const resp = await fetch(`http://127.0.0.1:${SIDECAR_PORT}/notify`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        session_id: "mock-channel:default:unknown_user",
        text: "notice",
      }),
    });
    expect(resp.status).toBe(200);
    const body = (await resp.json()) as any;
    expect(body.delivered).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// Edge case: /send endpoint
// ---------------------------------------------------------------------------

describe("/send edge cases", () => {
  it("returns 400 when required fields are missing", async () => {
    const resp = await fetch(`http://127.0.0.1:${SIDECAR_PORT}/send`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ channel: "mock-channel" }),
    });
    expect(resp.status).toBe(400);
    const body = (await resp.json()) as any;
    expect(body.error).toContain("missing");
  });

  it("returns 404 for unknown channel", async () => {
    const resp = await fetch(`http://127.0.0.1:${SIDECAR_PORT}/send`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        channel: "nonexistent",
        to: "user_1",
        text: "hello",
      }),
    });
    expect(resp.status).toBe(404);
    const body = (await resp.json()) as any;
    expect(body.error).toContain("nonexistent");
    expect(body.available).toBeDefined();
  });

  it("sends message to valid channel with default account", async () => {
    const resp = await fetch(`http://127.0.0.1:${SIDECAR_PORT}/send`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        channel: "mock-channel",
        to: "user_send",
        text: "direct send",
      }),
    });
    expect(resp.status).toBe(200);
    expect(sentMessages).toHaveLength(1);
    expect(sentMessages[0].text).toBe("direct send");
    expect(sentMessages[0].chatId).toBe("user_send");
    expect(sentMessages[0].accountId).toBe("default");
  });
});

// ---------------------------------------------------------------------------
// Edge case: typing indicator races
// ---------------------------------------------------------------------------

describe("typing indicator edge cases", () => {
  it("endPendingTyping is no-op when no typing state exists", async () => {
    const { endPendingTyping: endTyping } = await import("../src/runtime.js");
    // Should not throw.
    await endTyping({ sessionId: "nonexistent:session:id", channelPlugin: mockChannel });
    expect(pendingTyping.has("nonexistent:session:id")).toBe(false);
  });

  it("endPendingTyping called twice for same session is idempotent", async () => {
    const { endPendingTyping: endTyping } = await import("../src/runtime.js");

    pendingTyping.set("mock-channel:default:double_end", {
      typingState: { reaction: "thinking" },
      cfg: { channels: {} },
      accountId: "default",
    });

    await endTyping({ sessionId: "mock-channel:default:double_end", channelPlugin: mockChannel });
    await endTyping({ sessionId: "mock-channel:default:double_end", channelPlugin: mockChannel });

    expect(pendingTyping.has("mock-channel:default:double_end")).toBe(false);
    // Only one cleanup call — second endTyping should be a no-op.
    expect(cleanupCalls).toHaveLength(1);
  });
});
