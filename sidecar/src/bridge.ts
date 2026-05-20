import express from "express";
import rateLimit from "express-rate-limit";
import type { SidecarConfig } from "./config.js";
import { logger } from "./log.js";

const log = logger("bridge");
import type {
  ChannelPlugin,
  EliChannelMessage,
  InboundEnvelope,
  SessionContext,
  ToolCallLifecycleEvent,
} from "./types.js";
import { emitPluginEvent } from "./api.js";
import { outboundMediaItems, resolveBridgeContractVersion } from "./contract.js";
import { envelopeToEliMessage, parseOutboundTarget } from "./envelope.js";
import { registry } from "./registry.js";
import { endPendingTyping, sessionContexts } from "./runtime.js";

// ---------------------------------------------------------------------------
// Tool grouping — infer group from tool name prefix
// ---------------------------------------------------------------------------

/**
 * Infer a tool group from the tool name.
 * 3+ segments: first two (feishu_calendar_event → feishu-calendar)
 * 1-2 segments: first only (feishu_oauth → feishu)
 */
function inferToolGroup(name: string): string {
  const parts = name.split("_");
  if (parts.length >= 3) return `${parts[0]}-${parts[1]}`;
  return parts[0];
}

// ---------------------------------------------------------------------------
// Inbound: channel plugin message → POST to eli
// ---------------------------------------------------------------------------

let eliUrl = "";
let sidecarConfig: SidecarConfig;
const INBOUND_RETRY_LIMIT = 3;
const INBOUND_RETRY_DELAY_MS = 200;

export function initBridge(config: SidecarConfig) {
  eliUrl = config.eli_url;
  sidecarConfig = config;
}

function extractCauseCode(err: unknown): string | null {
  if (!err || typeof err !== "object" || !("cause" in err)) return null;
  const cause = (err as { cause?: unknown }).cause;
  if (!cause || typeof cause !== "object" || !("code" in cause)) return null;
  const code = (cause as { code?: unknown }).code;
  return typeof code === "string" ? code : null;
}

function isRetryableInboundError(err: unknown): boolean {
  return extractCauseCode(err) === "ECONNREFUSED";
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function postToEli(url: string, msg: EliChannelMessage): Promise<Response> {
  return fetch(url, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(msg),
  });
}

async function handleInboundResponse(url: string, resp: Response): Promise<boolean> {
  if (resp.ok) return true;
  const body = await resp.text();
  log.error("POST failed", { url, status: resp.status, body });
  return true;
}

function handleInboundError(url: string, err: unknown, retry: number): boolean {
  if (isRetryableInboundError(err) && retry < INBOUND_RETRY_LIMIT) {
    log.warn("POST ECONNREFUSED, retrying", { url, retry: retry + 1, max: INBOUND_RETRY_LIMIT });
    return false;
  }
  log.error("POST error", { url, err: String(err) });
  return true;
}

async function trySendToEli(url: string, msg: EliChannelMessage, retry: number): Promise<boolean> {
  try {
    const resp = await postToEli(url, msg);
    return await handleInboundResponse(url, resp);
  } catch (err) {
    return handleInboundError(url, err, retry);
  }
}

function buildChannelConfig(): { channels: SidecarConfig["channels"] } {
  return { channels: sidecarConfig.channels };
}

function normalizeToolCallText(text: string | null | undefined): string | null {
  if (typeof text !== "string") return null;
  const trimmed = text.trim();
  return trimmed.length > 0 ? trimmed : null;
}

function extractToolDescription(body: any): string | undefined {
  const description = normalizeToolCallText(body?.description);
  return description ?? undefined;
}

function invalidContractVersion(version: unknown): string | null {
  try {
    resolveBridgeContractVersion(version);
    return null;
  } catch (err: any) {
    return err?.message ?? "unsupported contract_version";
  }
}

function outboundMediaWithPath(msg: EliChannelMessage) {
  return outboundMediaItems(msg).flatMap((item) =>
    typeof item.path === "string" && item.path.length > 0
      ? [{ path: item.path, media_type: item.media_type, mime_type: item.mime_type ?? "" }]
      : []
  );
}

async function resolveToolCallText(
  channelPlugin: ChannelPlugin,
  event: ToolCallLifecycleEvent,
): Promise<string | null> {
  if (channelPlugin.lifecycle?.renderToolCallText) {
    return normalizeToolCallText(await channelPlugin.lifecycle.renderToolCallText(event));
  }
  if (event.phase !== "before") {
    return null;
  }
  return normalizeToolCallText(event.description);
}

function resolveToolNoticeTarget(
  channelPlugin: ChannelPlugin,
  sessionCtx: SessionContext,
): string {
  return (
    sessionCtx.channelTarget ||
    channelPlugin.lifecycle?.resolveOutboundTarget?.({}, sessionCtx.chatId) ||
    sessionCtx.chatId
  );
}

async function notifyToolCall(
  channelPlugin: ChannelPlugin | undefined,
  sessionCtx: SessionContext | null,
  event: ToolCallLifecycleEvent,
): Promise<void> {
  if (!sessionCtx || !channelPlugin?.outbound?.sendText) {
    return;
  }

  const text = await resolveToolCallText(channelPlugin, event);
  if (!text) {
    return;
  }

  await sendSessionNotice(channelPlugin, sessionCtx, text, `tool "${event.toolName}"`);
}

async function sendSessionNotice(
  channelPlugin: ChannelPlugin | undefined,
  sessionCtx: SessionContext | null,
  text: string,
  noticeKind = "session notice",
): Promise<void> {
  if (!sessionCtx || !channelPlugin?.outbound?.sendText) {
    return;
  }

  try {
    await channelPlugin.outbound.sendText({
      cfg: buildChannelConfig(),
      to: resolveToolNoticeTarget(channelPlugin, sessionCtx),
      text,
      accountId: sessionCtx.accountId,
      // Channels that track per-turn state (e.g. feishu-cli's inflight
      // batch for quote-reply + Typing reaction cleanup) must treat
      // notices as side-effects that don't conclude the turn.
      kind: "notice",
    });
  } catch (err: any) {
    log.error("send failed", { kind: noticeKind, err: err?.message ?? String(err) });
  }
}

/**
 * Called by channel gateways when they receive a message.
 * Normalizes the envelope and POSTs it to eli's webhook channel.
 */
export async function sendToEli(envelope: InboundEnvelope): Promise<void> {
  const msg = envelopeToEliMessage(envelope);
  const url = `${eliUrl}/inbound`;

  for (let retry = 0; retry <= INBOUND_RETRY_LIMIT; retry += 1) {
    if (await trySendToEli(url, msg, retry)) return;
    await sleep(INBOUND_RETRY_DELAY_MS);
  }
}

// ---------------------------------------------------------------------------
// Outbound: eli callback → route to channel plugin
// ---------------------------------------------------------------------------

/**
 * Start the outbound HTTP server that eli calls back to with responses.
 */
export function startOutboundServer(port: number): Promise<import("node:http").Server> {
  return new Promise((resolve) => {
    const app = express();
    app.use(express.json());

    // Auth middleware: validate ELI_SIDECAR_TOKEN when set
    const sidecarToken = process.env.ELI_SIDECAR_TOKEN;
    if (sidecarToken) {
      app.use((req, res, next) => {
        if (req.path === "/health") return next();
        const auth = req.headers.authorization;
        if (auth !== `Bearer ${sidecarToken}`) {
          res.status(401).json({ error: "unauthorized" });
          return;
        }
        next();
      });
    }

    app.post("/outbound", async (req, res) => {
      const msg = req.body as EliChannelMessage;
      const contractError = invalidContractVersion(msg.contract_version);
      if (contractError) {
        res.status(400).json({ error: contractError });
        return;
      }
      const cleanupOnly = Boolean(msg.context?._eli_cleanup_only);
      let { sourceChannel, accountId, chatId, chatType } = parseOutboundTarget(msg);

      // Fallback: extract source_channel from session_id (format: "channel:account:chatId")
      if (!sourceChannel && msg.session_id) {
        const parts = msg.session_id.split(":");
        if (parts.length >= 2 && parts[0]) {
          sourceChannel = parts[0];
          accountId = accountId || parts[1];
          chatId = chatId || parts.slice(2).join(":");
        }
      }

      if (!sourceChannel) {
        log.error("outbound: cannot determine source_channel from context or session_id");
        res.status(400).json({ error: "missing source_channel" });
        return;
      }

      const channelPlugin = registry.channels.get(sourceChannel);
      if (!channelPlugin) {
        log.error("outbound: unknown channel", { channel: sourceChannel });
        res.status(404).json({ error: `channel "${sourceChannel}" not found` });
        return;
      }

      if (!cleanupOnly && !channelPlugin.outbound?.sendText) {
        log.error("outbound: channel has no sendText", { channel: sourceChannel });
        res.status(501).json({ error: `channel "${sourceChannel}" cannot send text` });
        return;
      }
      const sendText = channelPlugin.outbound?.sendText;
      const sendMedia = channelPlugin.outbound?.sendMedia;

      try {
        // Build the cfg object in the shape OpenClaw plugins expect:
        //   { channels: { feishu: { appId, appSecret, accounts: { default: {...} } } } }
        const cfg = { channels: sidecarConfig.channels };

        // Route target: use lifecycle hook, or fall back to context fields / chatId.
        let to = chatId;
        if (!cleanupOnly) {
          if (channelPlugin.lifecycle?.resolveOutboundTarget) {
            to = channelPlugin.lifecycle.resolveOutboundTarget(msg.context ?? {}, chatId);
          } else {
            to = msg.context?.channel_target || msg.context?.feishu_to || chatId;
          }
        }

        const mediaItems = outboundMediaWithPath(msg);

        // Mid-turn dispatches (the `message.send` tool, the greeting on
        // new-session join, etc.) reach /outbound with the same shape as
        // the final reply but mark themselves via context. Treat them as
        // notices so channel plugins preserve their per-turn state and
        // typing indicators stay until the actual answer ships.
        const midTurn = Boolean(msg.context?._eli_mid_turn);

        log.debug("outbound", {
          channel: sourceChannel, to, text_len: msg.content?.length,
          media: mediaItems.length, cleanup: cleanupOnly, mid_turn: midTurn,
        });

        // Only clear the per-session typing indicator on a FINAL reply (or
        // an explicit cleanup-only call). Mid-turn notices must leave the
        // indicator in place so the user still sees "bot is typing" until
        // the real answer arrives. Cleanup stays off the critical path so
        // outbound delivery isn't blocked on plugin-side reaction APIs.
        const sessionId = msg.session_id || `${sourceChannel}:${accountId}:${chatId}`;
        if (!midTurn) {
          void endPendingTyping({ sessionId, channelPlugin });
        }

        if (cleanupOnly) {
          res.json({ ok: true, cleanup_only: true });
          return;
        }
        if (!sendText) {
          res.status(501).json({ error: `channel "${sourceChannel}" cannot send text` });
          return;
        }

        // Send text — original path, errors propagate to caller.
        const result = await sendText({
          cfg,
          to,
          text: msg.content,
          accountId,
          kind: midTurn ? "notice" : "final",
        });

        // Send media independently — fire-and-forget, errors logged but don't fail the response.
        if (mediaItems.length > 0 && sendMedia) {
          for (const item of mediaItems) {
            const usesMediaUrl = sourceChannel === "openclaw-lark" || sourceChannel === "feishu" || sourceChannel === "discord";
            const mediaParams = usesMediaUrl
              ? { cfg, to, text: undefined, mediaUrl: item.path, mediaLocalRoots: ["/tmp"], accountId }
              : {
                  text: undefined,
                  mediaPath: item.path,
                  mediaType: item.media_type,
                  target: { chatId: to, chatType: chatType ?? "direct" },
                  config: cfg,
                  accountId,
                };
            sendMedia(mediaParams as any).catch((err: any) => {
              log.error("outbound sendMedia error", { path: item.path, err: String(err) });
            });
          }
        }

        res.json(result);
      } catch (err: any) {
        log.error("outbound sendText error", { err: String(err) });
        res.status(500).json({ error: err.message ?? "sendText failed" });
      }
    });

    // Tool listing — returns tool names, descriptions, parameter schemas, and group.
    app.get("/tools", (_req, res) => {
      const tools = Array.from(registry.tools.values()).map((t) => ({
        name: t.name,
        description: t.description,
        parameters: t.parameters,
        group: t.group ?? inferToolGroup(t.name),
      }));
      res.json(tools);
    });

    // Tool execution — calls the registered tool by name.
    // Accepts { params, context?, session_id? } where context carries session info
    // for constructing channel-specific auth context.
    app.post("/tools/:name", async (req, res) => {
      const tool = registry.tools.get(req.params.name);
      if (!tool) {
        res.status(404).json({ error: `tool "${req.params.name}" not found` });
        return;
      }

      const params = req.body?.params ?? req.body ?? {};
      const id = req.body?.id ?? `call_${Date.now()}`;

      // Find session context: prefer explicit session_id, fall back to most recent.
      let sessionCtx: SessionContext | null = null;
      const requestedSession = req.body?.session_id ?? req.body?.context?.session_id;
      if (requestedSession) {
        sessionCtx = sessionContexts.get(requestedSession) ?? null;
      }
      // No fallback — require explicit session_id to prevent cross-user
      // auth leakage in multi-session scenarios.

      // Synthetic session for external agents that pass `channel` instead of session_id.
      if (!sessionCtx && req.body?.channel) {
        const ch = req.body.channel as string;
        const plugin = registry.channels.get(ch);
        if (plugin) {
          sessionCtx = {
            channel: ch,
            accountId: (req.body?.account_id ?? "default") as string,
            chatId: "",
            senderId: "external-agent",
            messageId: "",
            chatType: "p2p",
            cfg: { channels: sidecarConfig.channels },
          };
        }
      }

      const channelPlugin = sessionCtx ? registry.channels.get(sessionCtx.channel) : undefined;
      const description = extractToolDescription(req.body);
      const startedAt = Date.now();

      const emitToolLifecycle = async (
        phase: "before" | "after",
        extras: Partial<ToolCallLifecycleEvent> = {},
      ) => {
        if (!sessionCtx) return;
        const event: ToolCallLifecycleEvent = {
          phase,
          toolName: tool.name,
          params,
          session: sessionCtx,
          description,
          ...extras,
        };
        await emitPluginEvent(`${phase}_tool_call`, event);
        await notifyToolCall(channelPlugin, sessionCtx, event);
      };

      await emitToolLifecycle("before");

      try {
        let result;
        if (sessionCtx) {
          // Try lifecycle hook first, then legacy fallback.
          if (channelPlugin?.lifecycle?.wrapToolExecution) {
            result = await channelPlugin.lifecycle.wrapToolExecution(sessionCtx, () =>
              tool.execute(id, params)
            );
          } else if (sessionCtx.channel === "feishu") {
            // Legacy fallback: LarkTicket wrapping (feishu only).
            try {
              const pluginDir = require("path").dirname(require.resolve("@larksuite/openclaw-lark"));
              const { withTicket } = require(pluginDir + "/src/core/lark-ticket.js");
              const ticket = {
                messageId: sessionCtx.messageId,
                chatId: sessionCtx.chatId,
                accountId: sessionCtx.accountId,
                senderOpenId: sessionCtx.senderId,
                chatType: sessionCtx.chatType,
                startTime: Date.now(),
              };
              result = await withTicket(ticket, () => tool.execute(id, params));
            } catch {
              result = await tool.execute(id, params);
            }
          } else {
            result = await tool.execute(id, params);
          }
        } else {
          result = await tool.execute(id, params);
        }

        await emitToolLifecycle("after", {
          durationMs: Date.now() - startedAt,
          result,
        });
        res.json(result);
      } catch (err: any) {
        const errorMessage = err?.message ?? "tool execution failed";
        await emitToolLifecycle("after", {
          durationMs: Date.now() - startedAt,
          error: errorMessage,
        });
        log.error("tool error", { tool: req.params.name, err: String(err) });
        res.status(500).json({
          content: [{ type: "text", text: `Error: ${errorMessage}` }],
        });
      }
    });

    app.post("/notify", async (req, res) => {
      const text = normalizeToolCallText(req.body?.text);
      if (!text) {
        res.status(400).json({ error: "missing text" });
        return;
      }

      const requestedSession = req.body?.session_id;
      if (!requestedSession) {
        res.status(400).json({ error: "missing session_id" });
        return;
      }

      const sessionCtx = sessionContexts.get(requestedSession) ?? null;
      if (!sessionCtx) {
        res.json({ ok: true, delivered: false });
        return;
      }

      const channelPlugin = registry.channels.get(sessionCtx.channel);
      await sendSessionNotice(channelPlugin, sessionCtx, text, "tool notice");
      res.json({ ok: true, delivered: true });
    });

    // Rate-limit setup endpoints (auth operations).
    const setupLimiter = rateLimit({ windowMs: 60_000, max: 5, standardHeaders: true });

    // Setup: start QR login for a channel.
    app.post("/setup/:channel/start", setupLimiter, async (req, res) => {
      const channelId = req.params.channel as string;
      const plugin = registry.channels.get(channelId);
      if (!plugin) {
        res.status(404).json({ error: `channel "${channelId}" not found` });
        return;
      }
      if (!(plugin.gateway as any)?.loginWithQrStart) {
        res.status(501).json({ error: `channel "${channelId}" does not support QR login` });
        return;
      }
      try {
        const result = await (plugin.gateway as any).loginWithQrStart({
          accountId: req.body?.accountId,
          force: req.body?.force ?? false,
        });
        res.json(result);
      } catch (err: any) {
        res.status(500).json({ error: err.message ?? "login start failed" });
      }
    });

    // Setup: wait for QR scan result.
    app.post("/setup/:channel/wait", setupLimiter, async (req, res) => {
      const channelId = req.params.channel as string;
      const plugin = registry.channels.get(channelId);
      if (!plugin) {
        res.status(404).json({ error: `channel "${channelId}" not found` });
        return;
      }
      if (!(plugin.gateway as any)?.loginWithQrWait) {
        res.status(501).json({ error: `channel "${channelId}" does not support QR login` });
        return;
      }
      const sessionKey = req.body?.sessionKey;
      if (!sessionKey) {
        res.status(400).json({ error: "sessionKey required" });
        return;
      }
      try {
        const result = await (plugin.gateway as any).loginWithQrWait({
          sessionKey,
          accountId: req.body?.accountId,
          timeoutMs: req.body?.timeoutMs ?? 300_000,
        });
        res.json(result);
      } catch (err: any) {
        res.status(500).json({ error: err.message ?? "login wait failed" });
      }
    });

    // Friendly send endpoint for external agents.
    // POST /send { channel, to, text, account_id? }
    app.post("/send", async (req, res) => {
      const { channel, to, text, account_id } = req.body ?? {};

      if (!channel || !to || !text) {
        res.status(400).json({ error: "missing required fields: channel, to, text" });
        return;
      }

      const channelPlugin = registry.channels.get(channel);
      if (!channelPlugin) {
        const available = Array.from(registry.channels.keys());
        res.status(404).json({ error: `channel "${channel}" not found`, available });
        return;
      }

      if (!channelPlugin.outbound?.sendText) {
        res.status(501).json({ error: `channel "${channel}" has no outbound adapter` });
        return;
      }

      const accountId = account_id ?? "default";
      const cfg = { channels: sidecarConfig.channels };

      try {
        const result = await channelPlugin.outbound.sendText({ cfg, to, text, accountId });
        res.json(result);
      } catch (err: any) {
        log.error("send error", { err: String(err) });
        res.status(500).json({ error: err?.message ?? "send failed" });
      }
    });

    // Health check.
    app.get("/health", (_req, res) => {
      const channels = Array.from(registry.channels.keys());
      const tools = Array.from(registry.tools.keys());
      res.json({ status: "ok", channels, tools });
    });

    const server = app.listen(port, () => {
      log.info("outbound server listening", { port });
      resolve(server);
    });
  });
}
