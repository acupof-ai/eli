/**
 * Built-in Feishu/Lark channel plugin that wraps `lark-cli`.
 *
 * Inbound pipeline:
 *   lark-cli event consume <EventKey>  (long-running NDJSON producer)
 *     → handleEventLine  (parse, dedup, build envelope, react)
 *     → enqueueForBatch  (per-chat debounce: combine rapid messages)
 *     → flushBatch       (combine items into one envelope)
 *     → enrichWithHistory (suffix-sliding window of recent chat history)
 *     → onMessage(envelope) — handoff to eli framework
 *
 * Outbound pipeline:
 *   bridge.ts /outbound  → sendText(params)
 *     → normalize escaped whitespace
 *     → takeInflight(chatId)  (the batch we were processing)
 *     → lark-cli im +messages-reply --message-id <latest>  (quote-reply UX)
 *     → deleteReaction(... for every item in the batch)  (clear Typing cue)
 *
 * Authentication is whatever lark-cli is logged into; the sidecar holds no
 * Feishu app credentials of its own.
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
// when another bus is already connected to the same app; without backoff
// we'd respawn every 3 s forever and spam the log.
const respawnAttempts = new Map<string, number>();

// ---------------------------------------------------------------------------
// Per-chat state for batching + reply quoting + reaction cleanup
// ---------------------------------------------------------------------------

/** One inbound message awaiting dispatch or outbound. */
export interface InboundItem {
  envelope: InboundEnvelope;
  messageId: string;
  receivedAt: number;
  /** Populated async after the reactions.create returns. */
  reactionId?: string;
}

/** A debounce window collecting rapid consecutive messages in one chat. */
interface QueuedBatch {
  items: InboundItem[];
  flushTimer: ReturnType<typeof setTimeout>;
  onMessage: (envelope: InboundEnvelope) => void | Promise<void>;
}

/** A dispatched batch waiting for the bot's reply to land. */
interface InflightBatch {
  items: InboundItem[];
  startedAt: number;
}

const queuedByChat = new Map<string, QueuedBatch>();
// Per-chat FIFO of dispatched batches awaiting their reply. A queue (not a
// single slot) is mandatory because eli serializes turns by session_id —
// rapid messages flush in order and complete in order, so we need batch_1's
// inflight entry to survive until batch_1's reply lands, even if batch_2
// is already queued. Single-slot was a P1 bug surfaced by codex review.
const inflightByChat = new Map<string, InflightBatch[]>();

/** Window during which consecutive messages collapse into one agent turn. */
const BATCH_DEBOUNCE_MS = 1500;
/** TTL for inflight batches — protects against an LLM run that never replies. */
const INFLIGHT_TTL_MS = 30 * 60 * 1000;

// ---------------------------------------------------------------------------
// Event dedup: lark-cli's bus replays the recent event log when its daemon
// reconnects, so without this every gateway restart re-fires the last N
// messages and the bot "spontaneously" answers history.
// ---------------------------------------------------------------------------

const seenEventIds = new Map<string, number>();
const SEEN_CAP = 2000;
const SEEN_TTL_MS = 24 * 60 * 60 * 1000;

export function alreadySeen(eventId: string | undefined, now = Date.now()): boolean {
  if (!eventId) return false;
  if (seenEventIds.size >= SEEN_CAP) {
    for (const [k, ts] of seenEventIds) {
      if (now - ts > SEEN_TTL_MS) seenEventIds.delete(k);
      if (seenEventIds.size < SEEN_CAP) break;
    }
    while (seenEventIds.size >= SEEN_CAP) {
      const oldest = seenEventIds.keys().next().value;
      if (oldest === undefined) break;
      seenEventIds.delete(oldest);
    }
  }
  if (seenEventIds.has(eventId)) return true;
  seenEventIds.set(eventId, now);
  return false;
}

/** For tests — wipes dedup state so each test starts fresh. */
export function __resetSeenEventIds(): void {
  seenEventIds.clear();
}

// ---------------------------------------------------------------------------
// Outbound text normalization
// ---------------------------------------------------------------------------

/**
 * Some upstream paths double-escape the LLM's string (JSON.stringify on text
 * that already contained 0x0A bytes), so what reaches us is the 2-char
 * sequence `\n` / `\t` / `\r` with zero real whitespace bytes. Feishu then
 * renders the literal backslash-letter. When the signature is unambiguous —
 * any escaped form present AND no real LF anywhere — unescape; mixed
 * content is left alone since it's probably the user's literal text.
 */
export function normalizeEscapedWhitespace(text: string): {
  text: string;
  changed: boolean;
} {
  if (text.includes("\n") || !/\\[ntr]/.test(text)) {
    return { text, changed: false };
  }
  const next = text
    .replace(/\\r\\n/g, "\n")
    .replace(/\\n/g, "\n")
    .replace(/\\r/g, "\r")
    .replace(/\\t/g, "\t");
  return { text: next, changed: true };
}

// ---------------------------------------------------------------------------
// Inbound — spawn `lark-cli event consume` per event key.
// ---------------------------------------------------------------------------

async function startGateway(params: GatewayStartParams): Promise<void> {
  const { accountId, onMessage } = params;
  const abortSignal: AbortSignal | undefined = (params as any).abortSignal;

  abortSignal?.addEventListener("abort", () => stopAccount(accountId));

  // Recycle any pre-existing lark-cli event bus before bringing up consumers.
  // Each gateway restart could leave a stale upstream subscription with
  // Feishu, manifesting as `online_instance_cnt > 1` and silent event drops.
  await resetEventBus();

  for (const eventKey of EVENT_KEYS) {
    spawnConsumer(accountId, eventKey, onMessage);
  }
  log.info("started feishu event consumers", { accountId, events: EVENT_KEYS });
}

async function resetEventBus(): Promise<void> {
  const result = await runLarkCli(["event", "stop", "--force"]);
  if (result.ok) {
    log.info("recycled lark-cli event bus before consumer start");
  } else {
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

  // IMPORTANT: lark-cli `event consume` treats stdin EOF as graceful exit
  // ("wired for AI subprocess callers"). Keep stdin as a pipe and never
  // close it — SIGTERM is the shutdown path. im.* events require --as bot.
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

  // Drop replays — see seenEventIds comment.
  if (alreadySeen(evt?.event_id)) {
    log.debug("dropping duplicate event", { eventId: evt.event_id, eventKey });
    return;
  }

  const envelope = toEnvelope(accountId, eventKey, evt);
  if (!envelope) return;

  const chatId = envelope.chatId as string | undefined;
  const messageId = envelope.messageId as string | undefined;
  if (!chatId || !messageId) {
    // Without these we can't reply-quote, react, or batch — pass through.
    void enrichAndDispatch(envelope, onMessage);
    return;
  }

  const item: InboundItem = { envelope, messageId, receivedAt: Date.now() };

  // Fire-and-forget Typing reaction; the resulting reaction_id is patched
  // back onto the queued item so the outbound path can clean it up.
  void ackInbound(item).catch((err) =>
    log.warn("ack failed", { messageId, err: err?.message }),
  );

  enqueueForBatch(chatId, item, onMessage);
}

/**
 * Add an inbound item to the per-chat debounce batch. Resets the flush
 * timer on each call so a burst of messages within BATCH_DEBOUNCE_MS lands
 * as a single agent turn with combined text.
 */
function enqueueForBatch(
  chatId: string,
  item: InboundItem,
  onMessage: (envelope: InboundEnvelope) => void | Promise<void>,
): void {
  const existing = queuedByChat.get(chatId);
  if (existing) {
    existing.items.push(item);
    clearTimeout(existing.flushTimer);
    existing.flushTimer = setTimeout(() => flushBatch(chatId), BATCH_DEBOUNCE_MS);
    return;
  }
  const batch: QueuedBatch = {
    items: [item],
    onMessage,
    flushTimer: setTimeout(() => flushBatch(chatId), BATCH_DEBOUNCE_MS),
  };
  queuedByChat.set(chatId, batch);
}

function flushBatch(chatId: string): void {
  const batch = queuedByChat.get(chatId);
  if (!batch) return;
  queuedByChat.delete(chatId);

  const combined = combineEnvelopes(batch.items);

  // Append to the per-chat inflight FIFO so the outbound path can pair the
  // reply with the right messages (latest in this batch for quote, all for
  // reaction cleanup). If a previous batch is still inflight for this chat,
  // we keep it — sendText shifts from the front so the oldest batch is the
  // one whose LLM run finishes first.
  const queue = inflightByChat.get(chatId) ?? [];
  queue.push({ items: batch.items, startedAt: Date.now() });
  inflightByChat.set(chatId, queue);

  void enrichAndDispatch(combined, batch.onMessage);
}

/**
 * Merge a burst of inbound items into one envelope. Uses the LATEST
 * envelope as the base (for messageId, sender, chatId, etc.) and prepends
 * a list of all messages chronologically into `text`, so the LLM sees the
 * full burst in one turn.
 */
export function combineEnvelopes(items: InboundItem[]): InboundEnvelope {
  if (items.length === 1) return items[0].envelope;

  const latest = items[items.length - 1].envelope;
  const lines = items
    .map((it, idx) => `[消息 ${idx + 1}/${items.length}] ${it.envelope.text ?? ""}`)
    .join("\n");

  return {
    ...latest,
    text: lines,
    batchSize: items.length,
    batchMessageIds: items.map((it) => it.messageId),
  };
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
 * `HISTORY_WINDOW + 1` messages (current included), filters out the
 * messages already in the current batch, reverses to chronological order,
 * and renders as plain lines.
 */
async function enrichWithHistory(env: InboundEnvelope): Promise<InboundEnvelope> {
  const chatId = env.chatId;
  if (!chatId) return env;

  // Build a set of message_ids that are part of the current dispatch — for
  // batched dispatches this is every item in the batch; for single-message
  // dispatches it's just the lone messageId. We exclude them from history
  // so the LLM sees them once (as the current turn), not twice.
  const currentIds = new Set<string>();
  const batched = (env as any).batchMessageIds as string[] | undefined;
  if (batched && batched.length > 0) {
    for (const id of batched) currentIds.add(id);
  } else if (env.messageId) {
    currentIds.add(env.messageId as string);
  }

  const result = await runLarkCli([
    "im", "+chat-messages-list",
    "--as", "bot",
    "--chat-id", chatId,
    "--page-size", String(HISTORY_WINDOW + Math.max(currentIds.size, 1)),
    "--sort", "desc",
  ]);
  if (!result.ok) return env;

  const messages: any[] =
    result.result?.data?.messages ??
    result.result?.messages ??
    [];

  const prior = messages
    .filter((m) => m && !currentIds.has(m.message_id))
    .slice(0, HISTORY_WINDOW)
    .reverse();

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
 * Acknowledge an inbound message with the Typing reaction so the user sees
 * the bot picked it up. Stashes the resulting reaction_id back on the item
 * so the outbound path can call reactions.delete to clear it.
 */
async function ackInbound(item: InboundItem): Promise<void> {
  const { messageId } = item;

  // Feishu's emoji_type enum is closed and case-sensitive. Full list at
  // https://open.feishu.cn/document/.../reference/im-v1/message-reaction/emojis-introduce
  // `Typing` (mixed case) = 正在输入/敲代码中.
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
  const reactionId = result.result?.data?.reaction_id;
  if (typeof reactionId === "string") {
    item.reactionId = reactionId;
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
    // Rust side flattens envelope.context into the user prompt as
    // `k=v|k=v|...`. Exposing message_id (and a few helpers) lets the
    // LLM cite the exact message when sending an upfront reply.
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

/**
 * Pop the oldest inflight batch for this chat — FIFO ordering matches eli's
 * per-session turn serialization (older batch's LLM finishes first, so its
 * reply arrives at sendText first). Expired entries at the front are
 * silently discarded so a long-dead LLM run doesn't poison fresh replies.
 */
function takeInflight(chatId: string): InflightBatch | undefined {
  const queue = inflightByChat.get(chatId);
  if (!queue || queue.length === 0) return undefined;
  const now = Date.now();
  while (queue.length > 0 && now - queue[0].startedAt > INFLIGHT_TTL_MS) {
    queue.shift();
  }
  const head = queue.shift();
  if (queue.length === 0) inflightByChat.delete(chatId);
  return head;
}

/** For tests — wipes inflight + queued state so each test starts fresh. */
export function __resetChannelState(): void {
  for (const batch of queuedByChat.values()) clearTimeout(batch.flushTimer);
  queuedByChat.clear();
  inflightByChat.clear();
}

/** For tests — seed an inflight batch directly. */
export function __seedInflight(chatId: string, items: InboundItem[]): void {
  const queue = inflightByChat.get(chatId) ?? [];
  queue.push({ items, startedAt: Date.now() });
  inflightByChat.set(chatId, queue);
}

/** For tests — number of inflight batches still pending for a chat. */
export function __inflightDepth(chatId: string): number {
  return inflightByChat.get(chatId)?.length ?? 0;
}

/** For tests — pop the head batch (same logic sendText uses). */
export function __takeInflightForTest(chatId: string): InflightBatch | undefined {
  return takeInflight(chatId);
}

export type { InflightBatch };

async function sendText(params: OutboundTextParams): Promise<OutboundResult> {
  const { to, replyToId } = params;
  let text = params.text;
  if (!text) return { ok: false, error: "empty text" };

  // Defuse upstream double-escaping that would otherwise render as literal
  // backslash-n in Feishu (esp. for diagrams / multi-line content).
  const normalized = normalizeEscapedWhitespace(text);
  if (normalized.changed) {
    log.warn("normalized double-escaped whitespace in outbound text", {
      sample: text.slice(0, 120),
    });
    text = normalized.text;
  }

  // Mid-turn notices (tool progress, /notify) emit through the same
  // outbound hook as final replies. They must NOT consume the pending
  // batch — otherwise the final answer loses its quote-reply target and
  // strands Typing reactions. Send as plain text, fresh message, no
  // inflight bookkeeping.
  if (params.kind === "notice") {
    return runLarkCli([
      "im", "+messages-send", "--as", "bot",
      ...routeArgs(to), "--text", text,
    ]);
  }

  // Quote-reply to the latest inbound in this batch so the bot's answer
  // threads visually under the user's message. FIFO shift: the oldest
  // inflight batch is the one whose LLM run finishes first, matching eli's
  // per-session turn serialization.
  const inflight = takeInflight(to);
  const latestMessageId = inflight?.items.at(-1)?.messageId;
  const targetMessageId = replyToId ?? latestMessageId;

  // `--as bot` for tenant_access_token; `--markdown` for native Lark
  // rendering of headings/bold/lists/code/inline-image-urls.
  const args = targetMessageId
    ? ["im", "+messages-reply", "--as", "bot", "--message-id", targetMessageId, "--markdown", text]
    : ["im", "+messages-send", "--as", "bot", ...routeArgs(to), "--markdown", text];

  const result = await runLarkCli(args);

  // Drop Typing reactions on every item in the batch. Best-effort.
  if (inflight) {
    for (const item of inflight.items) {
      if (item.reactionId) {
        void deleteReaction(item.messageId, item.reactionId).catch(() => {});
      }
    }
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
export function routeArgs(to: string): string[] {
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
// Helper for runtime registration.
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
    // runtime.ts — we handle reactions ourselves on the batch boundary.
    async onInboundMessage() { return null; },
    async onOutboundReply() { /* no-op */ },
    resolveOutboundTarget(_context, chatId) { return chatId; },
  },
};
