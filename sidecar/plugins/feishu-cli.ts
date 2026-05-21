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
// Bot identity — loaded once at startup so we can:
//   (1) drop messages the bot sent itself (open_id match)
//   (2) only respond in group chats when @-mentioned (name match)
// ---------------------------------------------------------------------------

interface BotIdentity {
  /** App display name as configured in the Feishu dev console (e.g. "eli"). */
  name: string | null;
  /** Bot's open_id, prefixed `ou_…`. */
  openId: string | null;
}

let botIdentity: BotIdentity = { name: null, openId: null };

/** Test-only: override / inspect bot identity. */
export function __setBotIdentity(next: BotIdentity): void {
  botIdentity = next;
}
export function __getBotIdentity(): BotIdentity {
  return { ...botIdentity };
}

async function loadBotIdentity(): Promise<void> {
  const res = await runLarkCli(["api", "GET", "/open-apis/bot/v3/info", "--as", "bot"]);
  if (!res.ok) {
    log.warn("could not load bot identity — group @-filter falls back to heuristic", {
      err: res.error,
    });
    return;
  }
  const bot = res.result?.bot ?? res.result?.data?.bot;
  if (bot) {
    botIdentity = {
      name: typeof bot.app_name === "string" ? bot.app_name : null,
      openId: typeof bot.open_id === "string" ? bot.open_id : null,
    };
    log.info("bot identity loaded", {
      name: botIdentity.name,
      openId: botIdentity.openId,
    });
  }
}

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
// Inbound text cleanup
// ---------------------------------------------------------------------------

/**
 * Remove Feishu @-mention noise from inbound text so the LLM prompt isn't
 * polluted with the bot's own name (or other users') as the leading token.
 *
 * Handles two shapes:
 *  - lark-cli pre-renders mentions as plain `@name ` at the start (most
 *    common case under our event consume path);
 *  - raw event payloads can still leak `<at user_id="...">@name</at>` if
 *    the consumer ever switches to non-rendered mode.
 *
 * Conservative: only strips leading mentions (1-3 stacked), never inline
 * mentions that the user wrote intentionally inside a sentence.
 */
export function stripMentions(text: string): string {
  // Strip raw <at>...</at> wrappers (and self-closing variants) up front.
  let s = text
    .replace(/<at\s+[^>]*>[^<]*<\/at>\s*/g, "")
    .replace(/<at\s+[^/>]*\/>\s*/g, "");

  // Strip leading @name tokens (max 3 to cover "@bot @secondary @third").
  // Match @ followed by non-whitespace, optional trailing whitespace; loop
  // so multiple leading mentions all peel.
  for (let i = 0; i < 3; i++) {
    const next = s.replace(/^@\S+\s*/, "");
    if (next === s) break;
    s = next;
  }
  return s.trim();
}

// ---------------------------------------------------------------------------
// Long reply chunking
// ---------------------------------------------------------------------------

/**
 * Feishu's post/markdown body cap is ~30 KB of UTF-8 *bytes*, not chars.
 * A 25 K-char CJK reply at 3 bytes/char is ~75 KB and would fail with an
 * opaque server error. Cap by bytes, prefer paragraph boundaries, and
 * never sever a fenced code block — if the cut would land inside one,
 * close the fence on this chunk and reopen it on the next.
 *
 * Returns one or more chunks (always ≥1; original text when ≤ cap).
 *
 * Backwards-compatible param name `MAX_CHUNK_CHARS` is kept for tests,
 * but the value is now interpreted as bytes everywhere it's used.
 */
export const MAX_CHUNK_CHARS = 25000;
export const MAX_CHUNK_BYTES = MAX_CHUNK_CHARS;

function utf8ByteLength(s: string): number {
  return Buffer.byteLength(s, "utf8");
}

/**
 * True when `senderId` looks like an app/bot identifier rather than a user.
 * Apps use `cli_<app_id>`; users use `ou_` (open_id), `on_` (union_id) or
 * a tenant `user_id`. We filter OUT apps rather than allow-listing one
 * user prefix so unusual deployments (union_id, tenant user_id) still work.
 */
export function isAppSender(senderId: string): boolean {
  return senderId.startsWith("cli_") || senderId.startsWith("app_");
}

function isSelfSender(senderId: string): boolean {
  return botIdentity.openId !== null && senderId === botIdentity.openId;
}

function groupMentionsBot(rawText: string): boolean {
  const name = botIdentity.name?.trim();
  if (name) return rawText.includes(`@${name}`);
  return stripMentions(rawText).length !== rawText.length;
}

/**
 * How many leading characters of `s` fit inside `capBytes` of UTF-8.
 * Returns s.length when the whole string fits.
 */
function charsFittingBytes(s: string, capBytes: number): number {
  if (utf8ByteLength(s) <= capBytes) return s.length;
  let bytes = 0;
  let i = 0;
  for (; i < s.length; i++) {
    const cb = utf8ByteLength(s[i]);
    if (bytes + cb > capBytes) break;
    bytes += cb;
  }
  return i;
}

/**
 * Count opening ``` fences (any backtick-3 run on its own line or
 * inline) up to `idx`. Odd count = unclosed fence currently open.
 */
function unclosedFenceTag(prefix: string): string | null {
  const matches = [...prefix.matchAll(/```([^\n`]*)/g)];
  if (matches.length === 0) return null;
  if (matches.length % 2 === 0) return null;
  // The last unclosed fence; its capture group is the language tag.
  const last = matches[matches.length - 1];
  return last[1] ?? "";
}

export function chunkText(text: string, capBytes = MAX_CHUNK_BYTES): string[] {
  if (utf8ByteLength(text) <= capBytes) return [text];

  const chunks: string[] = [];
  let remaining = text;
  let carryFence: string | null = null;

  while (utf8ByteLength(remaining) > capBytes) {
    // If a previous chunk left a fence open, prefix the continuation
    // with the same opener so the markdown stays well-formed.
    const prefix = carryFence !== null ? "```" + carryFence + "\n" : "";

    // Compute the max char count that fits the byte cap *after* the prefix.
    const innerCap = capBytes - utf8ByteLength(prefix);
    const maxChars = charsFittingBytes(remaining, Math.max(innerCap, 1));

    // Prefer paragraph break, then newline, then space — always in the
    // upper half of the available window so we don't emit tiny chunks.
    const lowerBound = Math.floor(maxChars / 2);
    let cut = remaining.lastIndexOf("\n\n", maxChars);
    if (cut < lowerBound) cut = remaining.lastIndexOf("\n", maxChars);
    if (cut < lowerBound) cut = remaining.lastIndexOf(" ", maxChars);
    if (cut < lowerBound) cut = maxChars;

    let body = remaining.slice(0, cut);
    // If the candidate body cuts inside an open code fence, close it now
    // and remember the language tag so the next chunk reopens cleanly.
    const stillOpen = unclosedFenceTag(prefix + body);
    if (stillOpen !== null) {
      body = body + "\n```";
      carryFence = stillOpen;
    } else {
      carryFence = null;
    }
    chunks.push(prefix + body);
    remaining = remaining.slice(cut).replace(/^[\s\n]+/, "");
  }

  if (remaining.length > 0) {
    const prefix = carryFence !== null ? "```" + carryFence + "\n" : "";
    chunks.push(prefix + remaining);
  }
  return chunks;
}

// ---------------------------------------------------------------------------
// Outbound error friendly-ization
// ---------------------------------------------------------------------------

/**
 * Convert raw eli error envelopes into a short human-readable line.
 *
 * The framework turns run_model failures into a textual final reply with
 * shape `[Error: run_model failed in plugin 'builtin': <kind>: <msg>]`.
 * Sending that verbatim to Feishu leaks provider names, plan tiers,
 * stack traces, and timestamps — useless to the user and embarrassing
 * in a group chat.
 *
 * Pattern is detected loosely (starts with `[Error:` and contains
 * `run_model`); when matched, return a friendly message that hints at
 * the cause (rate limit / overflow / unknown) without dumping internals.
 */
export function friendlyizeError(text: string): string {
  if (!text.startsWith("[Error:") || !text.includes("run_model")) return text;
  const lower = text.toLowerCase();
  if (lower.includes("usage_limit") || lower.includes("rate") || lower.includes("429")) {
    return "抱歉，当前模型限流了，过会儿再试一下。";
  }
  if (lower.includes("context") && lower.includes("overflow")) {
    return "对话太长超出了模型上下文窗口，换条新对话再问吧。";
  }
  if (lower.includes("timeout") || lower.includes("timed out")) {
    return "模型响应超时了，再发一次试试。";
  }
  return "抱歉，模型这次没回上来，过会儿再试一下。";
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
  await loadBotIdentity();

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
    // Routine lark-cli lifecycle chatter is noise once we trust the bus.
    // Surface errors / warnings at info; everything else at debug.
    const isRoutine =
      text.includes("[event] consuming as") ||
      text.includes("[event] listening") ||
      text.includes("[event] to stop gracefully") ||
      text.includes("[event] ready") ||
      text.includes("[event] local bus") ||
      text.includes("[event] started bus daemon") ||
      text.includes("[event] remote connection check");
    if (isRoutine) {
      log.debug("lark-cli event stderr", { eventKey, text: text.slice(0, 500) });
    } else {
      log.info("lark-cli event stderr", { eventKey, text: text.slice(0, 500) });
    }
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

  // Slash commands are handled in an async wrapper because the lark-cli
  // call has to be awaited — if it fails, we fall through to the normal
  // LLM dispatch path so the user is never left without any reply.
  void handleSlashOrDispatch(envelope, chatId, messageId, onMessage).catch((err) =>
    log.error("inbound dispatch threw", { messageId, err: err?.message }),
  );
}

async function handleSlashOrDispatch(
  envelope: InboundEnvelope,
  chatId: string,
  messageId: string,
  onMessage: (envelope: InboundEnvelope) => void | Promise<void>,
): Promise<void> {
  if (typeof envelope.text === "string") {
    const handled = await trySlashCommand(envelope.text, messageId);
    if (handled) return;
  }

  const item: InboundItem = { envelope, messageId, receivedAt: Date.now() };

  // Fire-and-forget Typing reaction; the resulting reaction_id is patched
  // back onto the queued item so the outbound path can clean it up.
  void ackInbound(item).catch((err) =>
    log.warn("ack failed", { messageId, err: err?.message }),
  );

  enqueueForBatch(chatId, item, onMessage);
}

const SLASH_HELP_TEXT = [
  "**Eli bot · 飞书通道**",
  "",
  "**支持的命令**",
  "- `/help` 显示这条帮助",
  "- `/status` 显示当前模型 + 心跳",
  "",
  "**对话**",
  "- 直接发消息即可，bot 看最近 5 条历史作为上下文。",
  "- 1.5 秒内连发多条会合并成一次回复。",
  "- 收到消息会立刻给一个 Typing 表情；回复完会撤掉。",
  "- 长回复会自动按段落拆成多条发送。",
  "",
  "项目: https://github.com/cklxx/eli",
].join("\n");

/**
 * Intercept canned slash commands. Returns true if the message was handled
 * (skip LLM dispatch); false to fall through to the normal pipeline.
 *
 * Awaits the lark-cli reply so a transient send failure falls back to
 * the LLM path instead of leaving the user with no response.
 */
async function trySlashCommand(text: string, messageId: string): Promise<boolean> {
  const cmd = text.trim().split(/\s+/, 1)[0]?.toLowerCase();
  let body: string | null = null;
  if (cmd === "/help" || cmd === "/?") {
    body = SLASH_HELP_TEXT;
  } else if (cmd === "/status") {
    body = renderStatus();
  }
  if (body === null) return false;
  const result = await runLarkCli([
    "im", "+messages-reply",
    "--as", "bot",
    "--message-id", messageId,
    "--markdown", body,
  ]);
  if (!result.ok) {
    log.warn("slash reply failed; falling back to LLM", { cmd, err: result.error });
    return false;
  }
  return true;
}

function renderStatus(): string {
  const up = process.uptime();
  const h = Math.floor(up / 3600);
  const m = Math.floor((up % 3600) / 60);
  const s = Math.floor(up % 60);
  return [
    "**Eli 状态**",
    `- uptime: ${h}h ${m}m ${s}s`,
    `- pid: ${process.pid}`,
    `- node: ${process.version}`,
    `- channels alive: feishu (lark-cli) + telegram + openclaw-weixin`,
    `- inflight chats: ${inflightByChat.size}`,
    `- queued batches: ${queuedByChat.size}`,
    `- seen events: ${seenEventIds.size}`,
  ].join("\n");
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

export function __inboundFromReceiveForTest(accountId: string, evt: any): InboundEnvelope | null {
  return toEnvelope(accountId, "im.message.receive_v1", evt);
}

function inboundFromReceive(accountId: string, evt: any): InboundEnvelope | null {
  if (!evt.chat_id || !evt.sender_id) return null;
  const senderId = String(evt.sender_id);
  // Anti-loop: reject messages whose sender is clearly an app/bot.
  // Feishu user senders use one of `ou_` (open_id), `on_` (union_id),
  // or a tenant `user_id`; apps/bots use `cli_<app_id>`. Filtering OUT
  // app ids (instead of allow-listing one user prefix) keeps the bot
  // safe from self-loops while still working under tenant_user_id /
  // union_id deployments.
  if (isSelfSender(senderId) || isAppSender(senderId)) return null;
  const chatType: "direct" | "group" = evt.chat_type === "p2p" ? "direct" : "group";
  const messageType = evt.message_type ?? evt.msg_type;
  if (messageType === "sticker") return null;
  const messageId = evt.message_id ?? evt.id;
  const rawText = typeof evt.content === "string" ? evt.content : "";
  if (chatType === "group" && !groupMentionsBot(rawText)) return null;
  return {
    channel: "feishu",
    accountId,
    senderId,
    chatType,
    chatId: evt.chat_id,
    text: stripMentions(rawText),
    messageId,
    eventId: evt.event_id,
    rawMessageType: messageType,
    // Rust side flattens envelope.context into the user prompt as
    // `k=v|k=v|...`. Exposing message_id (and a few helpers) lets the
    // LLM cite the exact message when sending an upfront reply.
    context: {
      message_id: messageId,
      sender_id: senderId,
      chat_type: chatType,
      msg_type: messageType,
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
  if (!text || !text.trim()) {
    // Whitespace-only outbound — the LLM produced effectively nothing.
    // Treat like a cleanup-only turn so the inflight FIFO doesn't stay
    // armed forever waiting for a reply that won't carry signal.
    if (params.kind !== "notice") {
      const inflight = takeInflight(to);
      if (inflight) {
        for (const item of inflight.items) {
          if (item.reactionId) {
            void deleteReaction(item.messageId, item.reactionId).catch(() => {});
          }
        }
      }
    }
    return { ok: false, error: "empty text" };
  }

  // Rewrite raw eli error dumps into something humans want to read in chat.
  // The framework converts run_model failures into a literal final reply
  // like `[Error: run_model failed in plugin 'builtin': ...]` which leaks
  // stack traces and provider quotas to the end user. Catch and soften.
  text = friendlyizeError(text);

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

  // Split long replies. The first chunk quote-replies and triggers the
  // reaction cleanup so the user sees a "the bot answered" signal even
  // before later chunks arrive; subsequent chunks send as fresh
  // non-quoted messages so the chat reads top-to-bottom in order.
  const chunks = chunkText(text);
  if (chunks.length > 1) {
    log.info("splitting long reply", {
      total_chars: text.length, chunks: chunks.length,
    });
  }

  // `--as bot` for tenant_access_token; `--markdown` for native Lark
  // rendering of headings/bold/lists/code/inline-image-urls.
  const firstArgs = targetMessageId
    ? ["im", "+messages-reply", "--as", "bot", "--message-id", targetMessageId, "--markdown", chunks[0]]
    : ["im", "+messages-send", "--as", "bot", ...routeArgs(to), "--markdown", chunks[0]];

  const result = await runLarkCli(firstArgs);

  // Drop Typing reactions on every item in the batch. Best-effort —
  // happens as soon as the first chunk lands so the visual indicator
  // matches "bot just spoke", not "bot's last chunk shipped".
  if (inflight) {
    for (const item of inflight.items) {
      if (item.reactionId) {
        void deleteReaction(item.messageId, item.reactionId).catch(() => {});
      }
    }
  }

  // Send remaining chunks in order. Failures on a continuation chunk
  // do not roll back the first chunk — the user has already seen the
  // start of the answer; better to leave that visible than to fail
  // silently or duplicate.
  for (let i = 1; i < chunks.length; i++) {
    const chunkResult = await runLarkCli([
      "im", "+messages-send", "--as", "bot",
      ...routeArgs(to), "--markdown", chunks[i],
    ]);
    if (!chunkResult.ok) {
      log.warn("chunk send failed", {
        index: i, total: chunks.length, err: chunkResult.error,
      });
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
    /**
     * Bridge invokes this when an inbound turn ends with no textual final
     * reply (e.g. eli's render_outbound produced an empty string and the
     * outbound was sent as cleanup-only). Drop the head inflight batch so
     * the next real turn isn't paired with a stale one, and tear down the
     * Typing reactions on the user's messages so the chat doesn't look
     * like the bot is still working.
     */
    async onTurnEnd({ chatId }) {
      const queue = inflightByChat.get(chatId);
      if (!queue || queue.length === 0) return;
      const batch = queue.shift();
      if (queue.length === 0) inflightByChat.delete(chatId);
      if (!batch) return;
      for (const item of batch.items) {
        if (item.reactionId) {
          void deleteReaction(item.messageId, item.reactionId).catch(() => {});
        }
      }
    },
  },
};
