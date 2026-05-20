/**
 * Built-in Feishu/Lark channel plugin that wraps `lark-cli`.
 *
 * Inbound: spawns `lark-cli event consume <EventKey>` per event key as a
 * long-running NDJSON producer. Each line is one event payload (schema from
 * `lark-cli event schema <key>`). Events are translated to InboundEnvelope
 * and dispatched into the gateway pipeline.
 *
 * Outbound: invokes `lark-cli im +messages-send` (or `+messages-reply` when
 * the eli envelope carries a reply target) as a short-lived child per call.
 *
 * Authentication is whatever lark-cli is logged into (`lark-cli auth status`).
 * The sidecar does not need feishu app credentials in its own config.
 */

import { spawn, spawnSync, type ChildProcess } from "node:child_process";
import type {
  ChannelPlugin,
  GatewayStartParams,
  InboundEnvelope,
  OutboundMediaParams,
  OutboundResult,
  OutboundTextParams,
} from "../src/types.js";
import { logger } from "../src/log.js";

const log = logger("feishu-cli");

// Which Feishu events to subscribe to. Each EventKey gets its own
// `lark-cli event consume` subprocess.
const EVENT_KEYS = [
  "im.message.receive_v1",
];

// Track active consumer children so abort signals can clean them up.
//   key = `${accountId}:${eventKey}`
const consumers = new Map<string, ChildProcess>();

// Per-consumer-key state for the respawn backoff. lark-cli refuses to start
// when another bus is already connected to the same app (typically on a
// different machine); without backoff we'd respawn every 3 s forever and
// spam the log. Resets to 0 once a consumer reports "ready".
const respawnAttempts = new Map<string, number>();

// ---------------------------------------------------------------------------
// Per-chat state: the latest inbound message_id (so the bot's reply quotes
// it via +messages-reply) and the reaction_id we added (so we can delete it
// once the bot's outbound has shipped).
//
// Keyed by chat_id. If two inbound messages arrive in the same chat before
// the bot replies to the first, the older entry is overwritten — the bot
// replies to the latest, which is what users expect, and the earlier
// reaction lingers (minor visual cost vs the storage complexity of a queue).
// ---------------------------------------------------------------------------
interface ChatPending {
  messageId: string;
  reactionId: string | null;
  addedAt: number;
}
const pendingByChat = new Map<string, ChatPending>();
const PENDING_TTL_MS = 30 * 60 * 1000; // 30 min

function rememberPending(chatId: string, messageId: string): void {
  pendingByChat.set(chatId, { messageId, reactionId: null, addedAt: Date.now() });
}

function takePending(chatId: string): ChatPending | undefined {
  const p = pendingByChat.get(chatId);
  if (!p) return undefined;
  if (Date.now() - p.addedAt > PENDING_TTL_MS) {
    pendingByChat.delete(chatId);
    return undefined;
  }
  pendingByChat.delete(chatId);
  return p;
}

// ---------------------------------------------------------------------------
// Event dedup: lark-cli replays the recent event log when its bus daemon
// reconnects (network blip, restart, etc.), so without this every restart
// re-fires the last N messages as if they were brand new and the bot
// "spontaneously" replies to history. Keep a bounded set of seen event_ids;
// older entries get evicted FIFO.
// ---------------------------------------------------------------------------
const seenEventIds = new Map<string, number>();
const SEEN_CAP = 2000;
const SEEN_TTL_MS = 24 * 60 * 60 * 1000;

function alreadySeen(eventId: string | undefined): boolean {
  if (!eventId) return false;
  const now = Date.now();
  // Lazy GC: prune expired entries when we'd otherwise exceed the cap.
  if (seenEventIds.size >= SEEN_CAP) {
    for (const [k, ts] of seenEventIds) {
      if (now - ts > SEEN_TTL_MS) seenEventIds.delete(k);
      if (seenEventIds.size < SEEN_CAP) break;
    }
    // Still over the cap? Drop the oldest insertions (Map iteration order).
    while (seenEventIds.size >= SEEN_CAP) {
      const first = seenEventIds.keys().next().value;
      if (first === undefined) break;
      seenEventIds.delete(first);
    }
  }
  if (seenEventIds.has(eventId)) return true;
  seenEventIds.set(eventId, now);
  return false;
}

// ---------------------------------------------------------------------------
// Inbound — spawn `lark-cli event consume` per event key.
// ---------------------------------------------------------------------------

async function startGateway(params: GatewayStartParams): Promise<void> {
  const { accountId, onMessage } = params;
  const abortSignal: AbortSignal | undefined = (params as any).abortSignal;

  abortSignal?.addEventListener("abort", () => stopAccount(accountId));

  // Recycle any pre-existing lark-cli event bus before bringing up consumers.
  // Each gateway restart may have left a stale upstream subscription with
  // Feishu (the bus tracks consumer disconnects but stale instances still
  // accumulate server-side, manifesting as `online_instance_cnt > 1` and
  // events being round-robined to the dead subscription — i.e. silent
  // message drops). `event stop --force` clears them; the next `event
  // consume` boots a fresh bus with a single clean upstream connection.
  await resetEventBus();

  for (const eventKey of EVENT_KEYS) {
    spawnConsumer(accountId, eventKey, onMessage);
  }
  log.info("started feishu event consumers", {
    accountId,
    events: EVENT_KEYS,
  });
}

async function resetEventBus(): Promise<void> {
  const result = await runLarkCli(["event", "stop", "--force"]);
  if (result.ok) {
    log.info("recycled lark-cli event bus before consumer start");
  } else {
    // `event stop` returns non-zero when there was nothing to stop — that's
    // the common path on a clean machine, so log at debug rather than warn.
    log.debug("event stop returned non-zero (likely no bus to stop)", {
      err: result.error,
    });
  }
}

function spawnConsumer(
  accountId: string,
  eventKey: string,
  onMessage: (envelope: InboundEnvelope) => void | Promise<void>,
): void {
  const key = `${accountId}:${eventKey}`;
  if (consumers.has(key)) return;

  // IMPORTANT: lark-cli `event consume` treats stdin EOF as a graceful exit
  // signal ("wired for AI subprocess callers"). We must keep stdin open as a
  // pipe (not "ignore") and never close it — SIGTERM is the shutdown path.
  // Event keys under im.* require bot identity; `--as auto` resolves to user.
  const child = spawn(
    "lark-cli",
    ["event", "consume", eventKey, "--quiet", "--as", "bot"],
    { stdio: ["pipe", "pipe", "pipe"] },
  );
  consumers.set(key, child);

  let buf = "";
  child.stdout.on("data", (chunk: Buffer) => {
    buf += chunk.toString("utf8");
    let idx: number;
    // eslint-disable-next-line no-cond-assign
    while ((idx = buf.indexOf("\n")) >= 0) {
      const line = buf.slice(0, idx);
      buf = buf.slice(idx + 1);
      if (!line.trim()) continue;
      handleEventLine(accountId, eventKey, line, onMessage);
    }
  });

  child.stderr.on("data", (chunk: Buffer) => {
    const text = chunk.toString("utf8").trim();
    if (!text) return;
    // "ready" line means the upstream subscription is live — clear backoff.
    if (text.includes("[event] ready")) {
      respawnAttempts.delete(key);
    }
    log.info("lark-cli event stderr", { eventKey, text: text.slice(0, 500) });
  });

  child.once("exit", (code, signal) => {
    consumers.delete(key);
    if (signal === "SIGTERM") return;
    const attempt = (respawnAttempts.get(key) ?? 0) + 1;
    respawnAttempts.set(key, attempt);
    // Exponential backoff capped at 60 s. Worst case: stuck "another bus
    // connected" on a peer machine — we keep retrying but quietly.
    const delayMs = Math.min(60_000, 3_000 * 2 ** (attempt - 1));
    if (attempt <= 2) {
      log.warn("lark-cli event consume exited; respawning", {
        eventKey, code, attempt, delayMs,
      });
    } else {
      log.debug("lark-cli event consume still failing; backing off", {
        eventKey, code, attempt, delayMs,
      });
    }
    setTimeout(() => spawnConsumer(accountId, eventKey, onMessage), delayMs);
  });
}

function handleEventLine(
  accountId: string,
  eventKey: string,
  line: string,
  onMessage: (envelope: InboundEnvelope) => void | Promise<void>,
): void {
  let evt: any;
  try {
    evt = JSON.parse(line);
  } catch (err: any) {
    log.warn("event JSON parse failed", { eventKey, snippet: line.slice(0, 200), err: err.message });
    return;
  }

  // Drop replays: lark-cli's bus replays recent events to a fresh consumer.
  // Without this the bot "auto-replies" to old messages every restart.
  if (alreadySeen(evt?.event_id)) {
    log.debug("dropping duplicate event", { eventId: evt.event_id, eventKey });
    return;
  }

  const envelope = toEnvelope(accountId, eventKey, evt);
  if (!envelope) return;

  // Remember the inbound so the eventual outbound can quote it (+messages-reply)
  // and so we can pair the reaction with its removal.
  if (envelope.chatId && envelope.messageId) {
    rememberPending(envelope.chatId as string, envelope.messageId as string);
  }

  // Fire-and-forget the Typing reaction; capture the reaction_id into the
  // pending state so the outbound path can delete it.
  void ackInbound(envelope).catch((err) =>
    log.warn("ack failed", { messageId: envelope.messageId, err: err?.message }),
  );

  // Pull a short suffix-sliding window of chat history so the LLM has
  // multi-turn context (eli's tape store only carries the bot's own past
  // outputs, not the rest of the chat). Awaiting this delays onMessage by
  // one quick lark-cli call (≈100-300 ms), worth it for context fidelity.
  void enrichAndDispatch(envelope, onMessage);
}

async function enrichAndDispatch(
  envelope: InboundEnvelope,
  onMessage: (envelope: InboundEnvelope) => void | Promise<void>,
): Promise<void> {
  let enriched = envelope;
  try {
    enriched = await enrichWithHistory(envelope);
  } catch (err: any) {
    log.warn("history enrichment failed; dispatching without context", {
      err: err?.message,
    });
  }
  try {
    await onMessage(enriched);
  } catch (err: any) {
    log.error("onMessage dispatch threw", { err: err.message });
  }
}

/** Suffix-sliding chat history window (last N messages preceding current). */
const HISTORY_WINDOW = 5;
/** Per-message text cap inside the history block, to keep prompt size sane. */
const HISTORY_TEXT_CAP = 200;

/**
 * Prepend a short suffix-sliding window of recent chat history to the
 * envelope text so the LLM sees multi-turn context. Pulls
 * `HISTORY_WINDOW + 1` messages (current included), filters the current
 * one out, reverses to chronological order, and renders as plain lines.
 *
 * Returns the envelope unmodified if the lookup fails or yields nothing —
 * the bot still gets the current message; only context is missing.
 */
async function enrichWithHistory(env: InboundEnvelope): Promise<InboundEnvelope> {
  const chatId = env.chatId;
  const messageId = env.messageId as string | undefined;
  if (!chatId) return env;

  const result = await runLarkCli([
    "im", "+chat-messages-list",
    "--as", "bot",
    "--chat-id", chatId,
    "--page-size", String(HISTORY_WINDOW + 1),
    "--sort", "desc",
  ]);
  if (!result.ok) return env;

  const messages: any[] =
    result.result?.data?.messages ??
    result.result?.messages ??
    [];

  const prior = messages
    .filter((m) => m && m.message_id !== messageId)
    .slice(0, HISTORY_WINDOW)
    .reverse(); // chronological: oldest first

  if (prior.length === 0) return env;

  const lines = prior.map(formatHistoryLine).join("\n");
  const enrichedText = [
    `[最近 ${prior.length} 条历史]`,
    lines,
    "",
    "[当前消息]",
    env.text,
  ].join("\n");

  return { ...env, text: enrichedText };
}

function formatHistoryLine(m: any): string {
  const role = m?.sender?.sender_type === "app" ? "助手" : "用户";
  const time = typeof m?.create_time === "string" ? m.create_time.slice(5) : "";
  let content = typeof m?.content === "string" ? m.content : JSON.stringify(m?.content ?? "");
  if (content.length > HISTORY_TEXT_CAP) content = content.slice(0, HISTORY_TEXT_CAP) + "…";
  const prefix = time ? `[${time}] ${role}` : role;
  return `${prefix}: ${content}`;
}

/**
 * Acknowledge an inbound message with a Feishu reaction so the user sees
 * the bot picked it up before the LLM finishes. No extra text message —
 * the reaction sits visibly on the user's message and the final answer
 * lands as the only bot reply.
 */
async function ackInbound(env: InboundEnvelope): Promise<void> {
  const messageId = env.messageId as string | undefined;
  if (!messageId) return;

  // Feishu's emoji_type enum is closed and case-sensitive. Full list at
  // https://open.feishu.cn/document/.../reference/im-v1/message-reaction/emojis-introduce
  // `Typing` (mixed case) = 正在输入/敲代码中 — fits the "bot is working" cue.
  const result = await runLarkCli([
    "im", "reactions", "create",
    "--as", "bot",
    "--params", JSON.stringify({ message_id: messageId }),
    "--data", JSON.stringify({ reaction_type: { emoji_type: "Typing" } }),
  ]);
  if (!result.ok) {
    log.warn("reaction create failed", { messageId, err: result.error });
    return;
  }
  // Stash the reaction_id back on the pending entry so the outbound path
  // can call reactions delete to clear it once the bot's reply has shipped.
  const reactionId = result.result?.data?.reaction_id;
  if (reactionId && env.chatId) {
    const pending = pendingByChat.get(env.chatId as string);
    if (pending && pending.messageId === messageId) {
      pending.reactionId = reactionId;
    }
  }
}

async function deleteReaction(messageId: string, reactionId: string): Promise<void> {
  const result = await runLarkCli([
    "im", "reactions", "delete",
    "--as", "bot",
    "--params", JSON.stringify({ message_id: messageId, reaction_id: reactionId }),
  ]);
  if (!result.ok) {
    log.debug("reaction delete failed (may already be gone)", {
      messageId, reactionId, err: result.error,
    });
  }
}

function toEnvelope(
  accountId: string,
  eventKey: string,
  evt: any,
): InboundEnvelope | null {
  if (!evt || typeof evt !== "object") return null;

  switch (eventKey) {
    case "im.message.receive_v1":
      return inboundFromReceive(accountId, evt);
    default:
      return null;
  }
}

function inboundFromReceive(accountId: string, evt: any): InboundEnvelope | null {
  if (!evt.chat_id || !evt.sender_id) return null;
  const chatType: "direct" | "group" = evt.chat_type === "p2p" ? "direct" : "group";
  const messageId = evt.message_id ?? evt.id;
  return {
    channel: "feishu",
    accountId,
    senderId: evt.sender_id,
    chatType,
    chatId: evt.chat_id,
    text: typeof evt.content === "string" ? evt.content : "",
    messageId,
    eventId: evt.event_id,
    rawMessageType: evt.message_type,
    // The Rust side flattens envelope.context into the user prompt as
    // `k=v|k=v|...`. Exposing message_id (and a few helpers) here lets the
    // LLM cite the exact message when sending an upfront reply via
    // `lark-cli im +messages-reply --message-id ...`.
    context: {
      message_id: messageId,
      sender_id: evt.sender_id,
      chat_type: chatType,
      msg_type: evt.message_type,
    },
  };
}

function stopAccount(accountId: string): void {
  for (const [key, child] of consumers) {
    if (!key.startsWith(`${accountId}:`)) continue;
    try { child.kill("SIGTERM"); } catch {}
    consumers.delete(key);
  }
}

// ---------------------------------------------------------------------------
// Outbound — `lark-cli im +messages-send` / `+messages-reply`.
// ---------------------------------------------------------------------------

async function sendText(params: OutboundTextParams): Promise<OutboundResult> {
  const { to, text, replyToId } = params;
  if (!text) return { ok: false, error: "empty text" };

  // Look up the latest inbound for this chat so the bot's reply can quote
  // the user's message via +messages-reply. Falls back to +messages-send
  // when we have no pending entry (cold start, restart with stale TTL, etc.).
  const pending = takePending(to);
  const targetMessageId = replyToId ?? pending?.messageId;

  // `--as bot` for tenant_access_token; `--markdown` for native Lark
  // rendering of headings/bold/lists/code/inline-image-urls.
  const args = targetMessageId
    ? ["im", "+messages-reply", "--as", "bot", "--message-id", targetMessageId, "--markdown", text]
    : ["im", "+messages-send", "--as", "bot", ...routeArgs(to), "--markdown", text];

  const result = await runLarkCli(args);

  // Drop the Typing reaction once the actual reply is out. Best-effort —
  // failure (already removed, race, etc.) is logged at debug.
  if (pending?.reactionId && pending.messageId) {
    void deleteReaction(pending.messageId, pending.reactionId).catch(() => {});
  }

  return result;
}

async function sendMedia(params: OutboundMediaParams | any): Promise<OutboundResult> {
  // Bridge may send media as { mediaUrl, to, cfg } (feishu legacy shape) or
  // as { target, mediaPath, mediaType } (generic shape).
  const target: string = params.target?.chatId ?? params.to ?? "";
  const path: string = params.mediaPath ?? params.mediaUrl ?? "";
  if (!target || !path) return { ok: false, error: "missing target or path" };

  const args = ["im", "+messages-send", "--as", "bot", ...routeArgs(target), "--file", path];
  return runLarkCli(args);
}

/** Build `--chat-id` or `--user-id` based on the prefix of the routing id. */
function routeArgs(to: string): string[] {
  if (to.startsWith("oc_")) return ["--chat-id", to];
  if (to.startsWith("ou_")) return ["--user-id", to];
  // Fall back to chat-id; lark-cli will return a clearer error than we can.
  return ["--chat-id", to];
}

function runLarkCli(args: string[]): Promise<OutboundResult> {
  return new Promise((resolve) => {
    const out: Buffer[] = [];
    const err: Buffer[] = [];
    const child = spawn("lark-cli", args, { stdio: ["ignore", "pipe", "pipe"] });
    child.stdout.on("data", (c: Buffer) => out.push(c));
    child.stderr.on("data", (c: Buffer) => err.push(c));
    child.once("error", (e) => resolve({ ok: false, error: e.message }));
    child.once("exit", (code) => {
      if (code === 0) {
        const stdout = Buffer.concat(out).toString("utf8");
        try {
          resolve({ ok: true, result: JSON.parse(stdout) });
        } catch {
          resolve({ ok: true, result: stdout });
        }
      } else {
        resolve({
          ok: false,
          error: Buffer.concat(err).toString("utf8").trim() || `lark-cli exit ${code}`,
        });
      }
    });
  });
}

// ---------------------------------------------------------------------------
// Helper for runtime registration — verify lark-cli is on PATH and authed.
// ---------------------------------------------------------------------------

export function isLarkCliAvailable(): boolean {
  const res = spawnSync("lark-cli", ["--version"], { stdio: "ignore" });
  return res.status === 0;
}

// ---------------------------------------------------------------------------
// Plugin export.
// ---------------------------------------------------------------------------

export const feishuCliPlugin: ChannelPlugin = {
  meta: {
    id: "feishu",
    label: "Feishu (lark-cli)",
    blurb: "Feishu IM via the official lark-cli (event consume + im send)",
  },
  config: {
    listAccountIds: () => ["default"],
    resolveAccount: () => ({}),
  },
  capabilities: {
    chatTypes: ["direct", "group"],
  },
  outbound: { sendText, sendMedia },
  gateway: { start: startGateway },
  lifecycle: {
    // Short-circuit the openclaw-lark "typing indicator" legacy paths in
    // runtime.ts — we don't reach into that plugin and reactions are optional.
    async onInboundMessage() { return null; },
    async onOutboundReply() { /* no-op */ },
    resolveOutboundTarget(_context, chatId) { return chatId; },
  },
};
