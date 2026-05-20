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

// ---------------------------------------------------------------------------
// Inbound — spawn `lark-cli event consume` per event key.
// ---------------------------------------------------------------------------

async function startGateway(params: GatewayStartParams): Promise<void> {
  const { accountId, onMessage } = params;
  const abortSignal: AbortSignal | undefined = (params as any).abortSignal;

  abortSignal?.addEventListener("abort", () => stopAccount(accountId));

  for (const eventKey of EVENT_KEYS) {
    spawnConsumer(accountId, eventKey, onMessage);
  }
  log.info("started feishu event consumers", {
    accountId,
    events: EVENT_KEYS,
  });
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
    if (text) log.info("lark-cli event stderr", { eventKey, text: text.slice(0, 500) });
  });

  child.once("exit", (code, signal) => {
    consumers.delete(key);
    if (signal === "SIGTERM") return;
    log.warn("lark-cli event consume exited; respawning in 3s", { eventKey, code });
    setTimeout(() => spawnConsumer(accountId, eventKey, onMessage), 3000);
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

  const envelope = toEnvelope(accountId, eventKey, evt);
  if (!envelope) return;

  try {
    void onMessage(envelope);
  } catch (err: any) {
    log.error("onMessage dispatch threw", { err: err.message });
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
  return {
    channel: "feishu",
    accountId,
    senderId: evt.sender_id,
    chatType,
    chatId: evt.chat_id,
    text: typeof evt.content === "string" ? evt.content : "",
    messageId: evt.message_id ?? evt.id,
    eventId: evt.event_id,
    rawMessageType: evt.message_type,
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

  const args = replyToId
    ? ["im", "+messages-reply", "--message-id", replyToId, "--text", text]
    : ["im", "+messages-send", ...routeArgs(to), "--text", text];

  return runLarkCli(args);
}

async function sendMedia(params: OutboundMediaParams | any): Promise<OutboundResult> {
  // Bridge may send media as { mediaUrl, to, cfg } (feishu legacy shape) or
  // as { target, mediaPath, mediaType } (generic shape).
  const target: string = params.target?.chatId ?? params.to ?? "";
  const path: string = params.mediaPath ?? params.mediaUrl ?? "";
  if (!target || !path) return { ok: false, error: "missing target or path" };

  const args = ["im", "+messages-send", ...routeArgs(target), "--file", path];
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
