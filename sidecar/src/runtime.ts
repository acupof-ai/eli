import { SidecarPluginApi } from "./api.js";
import { registry } from "./registry.js";
import { sendToEli } from "./bridge.js";
import type { SidecarConfig } from "./config.js";
import type {
  ChannelPlugin,
  InboundEnvelope,
  OpenClawPluginDefinition,
  SessionContext,
} from "./types.js";
import { logger, pluginLogger } from "./log.js";
import { mkdir, writeFile } from "node:fs/promises";
import path from "node:path";
import { buildRandomTempFilePath } from "openclaw/plugin-sdk/temp-path";

const log = logger("runtime");
const typingLog = logger("typing");
const skillsLog = logger("skills");

const MIME_TO_EXTENSION: Record<string, string> = {
  "application/pdf": ".pdf",
  "audio/mp4": ".m4a",
  "audio/mpeg": ".mp3",
  "audio/ogg": ".ogg",
  "audio/wav": ".wav",
  "image/bmp": ".bmp",
  "image/gif": ".gif",
  "image/jpeg": ".jpg",
  "image/jpg": ".jpg",
  "image/png": ".png",
  "image/svg+xml": ".svg",
  "image/tiff": ".tiff",
  "image/webp": ".webp",
  "text/plain": ".txt",
  "video/mp4": ".mp4",
  "video/quicktime": ".mov",
  "video/webm": ".webm",
};

function normalizeContentType(contentType?: string): string {
  return contentType?.split(";")[0]?.trim().toLowerCase() || "application/octet-stream";
}

function extensionFromContentType(contentType: string): string | undefined {
  return MIME_TO_EXTENSION[normalizeContentType(contentType)];
}

function extensionFromFileName(fileName?: string): string | undefined {
  const extension = fileName ? path.extname(path.basename(fileName)).toLowerCase() : "";
  return extension || undefined;
}

function startsWithBytes(buffer: Buffer, bytes: number[]): boolean {
  return bytes.every((byte, index) => buffer[index] === byte);
}

function looksLikeSvg(buffer: Buffer): boolean {
  return buffer.subarray(0, 256).toString("utf8").toLowerCase().includes("<svg");
}

function detectMimeFromBuffer(buffer?: Buffer): string {
  if (!buffer?.length) return "application/octet-stream";
  if (startsWithBytes(buffer, [0xff, 0xd8, 0xff])) return "image/jpeg";
  if (startsWithBytes(buffer, [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a])) return "image/png";
  if (startsWithBytes(buffer, [0x47, 0x49, 0x46, 0x38])) return "image/gif";
  if (startsWithBytes(buffer, [0x42, 0x4d])) return "image/bmp";
  if (startsWithBytes(buffer, [0x49, 0x49, 0x2a, 0x00])) return "image/tiff";
  if (startsWithBytes(buffer, [0x4d, 0x4d, 0x00, 0x2a])) return "image/tiff";
  if (buffer.toString("ascii", 0, 4) === "RIFF" && buffer.toString("ascii", 8, 12) === "WEBP") return "image/webp";
  return looksLikeSvg(buffer) ? "image/svg+xml" : "application/octet-stream";
}

async function detectMime(input: Buffer | { buffer?: Buffer }): Promise<string> {
  const buffer = Buffer.isBuffer(input) ? input : input?.buffer;
  return detectMimeFromBuffer(buffer);
}

async function saveMediaBuffer(
  buffer: Buffer,
  contentType: string,
  direction: string,
  maxBytes: number,
  fileName?: string,
): Promise<{ path: string; contentType: string }> {
  if (buffer.length > maxBytes) throw new Error(`media exceeds maxBytes: ${buffer.length} > ${maxBytes}`);
  const resolvedType = normalizeContentType(contentType) === "application/octet-stream"
    ? detectMimeFromBuffer(buffer)
    : normalizeContentType(contentType);
  const extension = extensionFromFileName(fileName) ?? extensionFromContentType(resolvedType);
  const filePath = buildRandomTempFilePath({ prefix: `${direction}-media`, extension });
  await mkdir(path.dirname(filePath), { recursive: true });
  await writeFile(filePath, buffer);
  return { path: filePath, contentType: resolvedType };
}

// ---------------------------------------------------------------------------
// Typing indicator state — keyed by session so outbound can clean up
// ---------------------------------------------------------------------------

/** Pending typing indicator states, keyed by session_id. */
export const pendingTyping = new Map<string, { typingState: any; cfg: any; accountId: string }>();

/**
 * Serialize typing lifecycle operations per session.
 *
 * Purpose:
 * The Feishu typing indicator is reaction-based, so "start typing" and
 * "stop typing" are two separate async API calls.
 *
 * Scenario:
 * `dispatchReplyFromConfig()` starts the typing reaction and immediately
 * forwards the inbound message to eli. The webhook `/inbound` handler only
 * enqueues that message and returns `200`, so the final outbound reply can
 * reach the sidecar before `addTypingIndicator()` has finished. Without a
 * queue, outbound cleanup checks `pendingTyping` too early, sees nothing,
 * and never calls remove.
 */
const typingQueues = new Map<string, Promise<void>>();

/** Per-session context — used by tool execution for channel-specific auth wrapping. */
export const sessionContexts = new Map<string, SessionContext>();

/** TTL for session context entries (30 minutes). */
const SESSION_CONTEXT_TTL_MS = 30 * 60 * 1000;

/** Track TTL timers so we can cancel the old one when a session refreshes. */
const sessionTimers = new Map<string, ReturnType<typeof setTimeout>>();

function queueTypingTask(sessionId: string, task: () => Promise<void>): Promise<void> {
  const previous = typingQueues.get(sessionId) ?? Promise.resolve();
  let tracked: Promise<void>;
  tracked = previous
    .catch(() => {})
    .then(task)
    .catch(() => {})
    .finally(() => {
      if (typingQueues.get(sessionId) === tracked) {
        typingQueues.delete(sessionId);
      }
    });
  typingQueues.set(sessionId, tracked);
  return tracked;
}

async function addTypingState(params: {
  channelPlugin?: ChannelPlugin;
  cfg: any;
  messageId: string;
  accountId: string;
  sessionId: string;
}): Promise<any> {
  const { channelPlugin, cfg, messageId, accountId, sessionId } = params;

  if (channelPlugin?.lifecycle?.onInboundMessage) {
    try {
      return await channelPlugin.lifecycle.onInboundMessage({ cfg, messageId, accountId, sessionId });
    } catch {
      return null;
    }
  }

  // Legacy fallback: only for feishu channel (the only one using Lark reactions).
  if (channelPlugin?.meta?.id !== "feishu") {
    return null;
  }

  try {
    const { addTypingIndicator } = require(
      require("path").dirname(require.resolve("@larksuite/openclaw-lark"))
        + "/src/messaging/outbound/typing.js"
    );
    const state = await addTypingIndicator({ cfg, messageId, accountId });
    if (state?.messageId && !state?.reactionId) {
      typingLog.debug("indicator added without reactionId", { messageId: state.messageId });
    }
    return state;
  } catch {
    return null;
  }
}

async function removeTypingState(
  channelPlugin: ChannelPlugin | undefined,
  typing: { typingState: any; cfg: any; accountId: string },
): Promise<void> {
  if (channelPlugin?.lifecycle?.onOutboundReply) {
    try {
      await channelPlugin.lifecycle.onOutboundReply({
        cfg: typing.cfg,
        typingState: typing.typingState,
        accountId: typing.accountId,
      });
    } catch (e: any) {
      typingLog.warn("lifecycle typing removal failed", { err: e.message });
    }
    return;
  }

  // Legacy fallback: only for feishu channel (the only one using Lark reactions).
  if (channelPlugin?.meta?.id !== "feishu") {
    return;
  }

  try {
    const pluginDir = require("path").dirname(require.resolve("@larksuite/openclaw-lark"));
    const { listReactionsFeishu, removeReactionFeishu } = require(
      pluginDir + "/src/messaging/outbound/reactions.js"
    );
    const messageId = typing.typingState?.messageId;
    const reactionId = typing.typingState?.reactionId;

    if (!messageId) {
      return;
    }

    if (reactionId) {
      await removeReactionFeishu({
        cfg: typing.cfg,
        messageId,
        reactionId,
        accountId: typing.accountId,
      });
      return;
    }

    const reactions = await listReactionsFeishu({
      cfg: typing.cfg,
      messageId,
      emojiType: "Typing",
      accountId: typing.accountId,
    });
    const appTypingReactions = reactions.filter((reaction: any) =>
      reaction?.operatorType === "app" && typeof reaction?.reactionId === "string" && reaction.reactionId.length > 0
    );

    if (appTypingReactions.length === 0) {
      typingLog.debug("cleanup skipped, no app-owned Typing reactions", { messageId });
      return;
    }

    for (const reaction of appTypingReactions) {
      await removeReactionFeishu({
        cfg: typing.cfg,
        messageId,
        reactionId: reaction.reactionId,
        accountId: typing.accountId,
      });
    }
    typingLog.debug("cleanup fallback removed reactions", { messageId, count: appTypingReactions.length });
  } catch (e: any) {
    typingLog.warn("indicator removal failed", { err: e.message });
  }
}

export function beginPendingTyping(params: {
  channelPlugin?: ChannelPlugin;
  cfg: any;
  messageId: string;
  accountId: string;
  sessionId: string;
}): Promise<void> {
  typingLog.debug("BEGIN", { session_id: params.sessionId, message_id: params.messageId });
  return queueTypingTask(params.sessionId, async () => {
    // Queue the add operation so a later cleanup for the same session cannot
    // overtake it and get dropped before the reaction state is recorded.
    const typingState = await addTypingState(params);
    if (!typingState) {
      typingLog.debug("BEGIN null state, skipping", { session_id: params.sessionId });
      pendingTyping.delete(params.sessionId);
      return;
    }
    typingLog.debug("BEGIN stored", { session_id: params.sessionId, msg_id: typingState?.messageId, rxn_id: typingState?.reactionId });
    pendingTyping.set(params.sessionId, {
      typingState,
      cfg: params.cfg,
      accountId: params.accountId,
    });
  });
}

export function endPendingTyping(params: {
  sessionId: string;
  channelPlugin?: ChannelPlugin;
}): Promise<void> {
  typingLog.debug("END", { session_id: params.sessionId, plugin: params.channelPlugin?.meta?.id ?? "none" });
  return queueTypingTask(params.sessionId, async () => {
    // Cleanup shares the same queue as beginPendingTyping(). If outbound
    // arrives before typing setup finishes, this waits behind the add step
    // and still removes the reaction once the state is available.
    const typing = pendingTyping.get(params.sessionId);
    if (!typing) {
      typingLog.debug("END not found in map", { session_id: params.sessionId });
      return;
    }
    typingLog.debug("END found", { session_id: params.sessionId, msg_id: typing.typingState?.messageId, rxn_id: typing.typingState?.reactionId });
    pendingTyping.delete(params.sessionId);
    try {
      await removeTypingState(params.channelPlugin, typing);
      typingLog.debug("END removal ok", { session_id: params.sessionId });
    } catch (e: any) {
      typingLog.warn("END removal threw", { session_id: params.sessionId, err: e.message });
    }
  });
}

// ---------------------------------------------------------------------------
// Plugin loading
// ---------------------------------------------------------------------------

/**
 * Build a minimal PluginRuntime that satisfies what channel plugins need.
 *
 * OpenClaw channel plugins call into runtime methods during their message
 * pipeline. We intercept dispatchReplyFromConfig to route messages to eli
 * instead of openclaw's agent runtime.
 */
function buildDiscordRuntimeNamespace(): Record<string, any> {
  try {
    const sdk = require("openclaw/plugin-sdk/discord");
    return {
      sendMessageDiscord: sdk.sendMessageDiscord,
      monitorDiscordProvider: sdk.monitorDiscordProvider,
      probeDiscord: sdk.probeDiscord,
      sendPollDiscord: sdk.sendPollDiscord,
      auditChannelPermissions: sdk.auditDiscordChannelPermissions,
      resolveChannelAllowlist: sdk.resolveDiscordChannelAllowlist,
      resolveUserAllowlist: sdk.resolveDiscordUserAllowlist,
      listDirectoryPeersLive: sdk.listDiscordDirectoryPeersLive,
      listDirectoryGroupsLive: sdk.listDiscordDirectoryGroupsLive,
      messageActions: sdk.discordMessageActions,
    };
  } catch (e: any) {
    log.debug("discord SDK not available", { err: e.message });
    return {};
  }
}

function buildPluginRuntime(config: SidecarConfig) {
  return {
    config: {
      loadConfig: () => ({
        channels: config.channels,
      }),
    },
    log: (...args: any[]) => log.info(args.map(String).join(" ")),
    error: (...args: any[]) => log.error(args.map(String).join(" ")),
    logging: {
      shouldLogVerbose: () => false,
      logInbound: () => {},
      logOutbound: () => {},
    },
    media: {
      detectMime,
    },
    channel: {
      discord: buildDiscordRuntimeNamespace(),
      reply: {
        /**
         * The main intercept point. When a channel plugin finishes processing
         * an inbound message, it calls this to dispatch to the "agent".
         * We redirect to eli instead.
         */
        dispatchReplyFromConfig: async (params: any) => {
          const ctx = params.ctx ?? {};
          // OpenClaw ctx uses PascalCase keys.
          // BodyForAgent has media paths substituted (e.g. ![image](/path/to/img.jpg))
          // and mention annotations — preferred for AI processing.
          // RawBody is the original text. Body has speaker prefix.
          const textBody = ctx.BodyForAgent ?? ctx.RawBody ?? ctx.Body ?? "";
          const senderId = ctx.SenderId ?? "";
          const senderName = ctx.SenderName ?? senderId;
          const to = ctx.To ?? "";  // "To" contains the chat target (e.g. channel:account:chatId)
          const sessionKey = ctx.SessionKey ?? "";
          const chatType = ctx.ChatType ?? "dm";
          const accountId = ctx.AccountId ?? "default";
          const channel = ctx.OriginatingChannel ?? "unknown";
          // Extract chatId from the "To" field (format: "channel:accountId:chatId" or just chatId)
          const chatId = to.includes(":") ? to.split(":").pop()! : to;

          log.debug("dispatch", { session: sessionKey, sender: senderName });

          // Save session context for tool execution.
          const messageId = ctx.MessageSid ?? ctx.MessageId ?? ctx.messageId ?? "";
          const sessionId = `${channel}:${accountId}:${chatId}`;
          const sessionCtx: SessionContext = {
            channel,
            senderId,
            chatId,
            channelTarget: to,
            accountId,
            messageId,
            chatType: chatType === "group" ? "group" : "p2p",
            cfg: params.cfg ?? {},
          };
          sessionContexts.set(sessionId, sessionCtx);
          // Cancel previous TTL timer to prevent it from deleting a refreshed entry.
          const prevTimer = sessionTimers.get(sessionId);
          if (prevTimer) clearTimeout(prevTimer);
          const timer = setTimeout(() => {
            sessionContexts.delete(sessionId);
            sessionTimers.delete(sessionId);
          }, SESSION_CONTEXT_TTL_MS);
          sessionTimers.set(sessionId, timer);

          // Pass the "To" field as-is so the outbound bridge can use it
          // as the channel route target for sendText.
          // Collect media file paths from the plugin's media resolution.
          const mediaPaths: string[] = [];
          if (ctx.MediaPaths && Array.isArray(ctx.MediaPaths)) {
            mediaPaths.push(...ctx.MediaPaths);
          } else if (ctx.MediaPath) {
            mediaPaths.push(ctx.MediaPath);
          }
          const mediaTypes: string[] = [];
          if (ctx.MediaTypes && Array.isArray(ctx.MediaTypes)) {
            mediaTypes.push(...ctx.MediaTypes);
          } else if (ctx.MediaType) {
            mediaTypes.push(ctx.MediaType);
          }

          const envelope: InboundEnvelope = {
            channel,
            accountId,
            senderId,
            senderName,
            chatType: chatType === "group" ? "group" : "direct",
            chatId,
            text: typeof textBody === "string" ? textBody : JSON.stringify(textBody),
            channel_target: to,  // e.g. "user:ou_xxx" or "channel:account:chatId"
            media_paths: mediaPaths.length > 0 ? mediaPaths : undefined,
            media_types: mediaTypes.length > 0 ? mediaTypes : undefined,
          };

          // Fire-and-forget typing indicator (don't block message forwarding).
          if (messageId) {
            const cfg = params.cfg ?? {};
            const channelPlugin = registry.channels.get(channel);
            void beginPendingTyping({ channelPlugin, cfg, messageId, accountId, sessionId });
          }

          await sendToEli(envelope);

          const dispatcher = params.dispatcher;
          if (dispatcher?.waitForIdle) {
            try { await dispatcher.waitForIdle(); } catch {}
          }

          return {
            queuedFinal: Promise.resolve(),
            counts: { sent: 1, queued: 0, dropped: 0 },
          };
        },
        resolveEnvelopeFormatOptions: (_cfg: any) => ({
          historyFormat: "plain",
          mentionFormat: "plain",
        }),
        formatAgentEnvelope: (ctx: any) => ctx,
        finalizeInboundContext: (ctx: any) => ctx,
        resolveHumanDelayConfig: () => ({ enabled: false, minMs: 0, maxMs: 0 }),
        createReplyDispatcherWithTyping: (_params: any) => ({
          dispatcher: {
            waitForIdle: async () => {},
            deliver: async () => {},
          },
          replyOptions: {},
          markDispatchIdle: () => {},
          markFullyComplete: () => {},
          abortCard: async () => {},
        }),
        withReplyDispatcher: async (params: any) => {
          if (params?.run) await params.run();
        },
        /**
         * System command dispatch (for /new, /reset, etc.).
         * Route to eli as well.
         */
        dispatchReplyWithBufferedBlockDispatcher: async (params: any) => {
          const ctx = params.ctx ?? {};
          const body = ctx.Body ?? ctx.body ?? ctx.rawBody ?? "";
          const senderId = ctx.SenderId ?? ctx.senderId ?? "";
          const chatId = ctx.ChatId ?? ctx.chatId ?? "";
          const channel = ctx.Channel ?? ctx.channel ?? "unknown";
          const accountId = ctx.AccountId ?? ctx.accountId ?? "default";

          log.debug("system command", { body });

          const envelope: InboundEnvelope = {
            channel,
            accountId,
            senderId,
            chatType: "direct",
            chatId,
            text: body,
          };
          await sendToEli(envelope);
        },
      },
      routing: {
        resolveAgentRoute: (params: any) => ({
          agentId: "main",
          sessionKey: `${params.channel ?? "channel"}:${params.accountId ?? "default"}:${params.chatId ?? "default"}`,
        }),
      },
      commands: {
        isControlCommandMessage: (text: string, _cfg: any) => {
          return /^\/(?:new|reset|stop|help)\s*$/i.test((text ?? "").trim());
        },
        shouldComputeCommandAuthorized: () => false,
        resolveCommandAuthorizedFromAuthorizers: () => true,
      },
      text: {
        resolveTextChunkLimit: () => 4000,
        resolveChunkMode: () => "plain",
        resolveMarkdownTableMode: () => "plain",
        chunkTextWithMode: (text: string) => [text],
        convertMarkdownTables: (text: string) => text,
        chunkMarkdownText: (text: string) => [text],
      },
      groups: {
        resolveGroupPolicy: () => "open",
        resolveRequireMention: () => false,
      },
      media: {
        saveMediaBuffer,
      },
      session: {
        resolveStorePath: (_store: any, _opts: any) => "/tmp/openclaw/sessions",
        recordInboundSession: async () => {},
      },
      pairing: {
        buildPairingReply: () => null,
        readAllowFromStore: () => ["*"],
        upsertPairingRequest: async () => {},
      },
    },
    system: {
      enqueueSystemEvent: (_msg: string, _data?: any) => {
        // No-op: system events are openclaw-internal.
      },
    },
  };
}

/**
 * Register built-in channel plugins when their config/env vars are present.
 */
function loadBuiltinPlugins(config: SidecarConfig): void {
  // Telegram: register if token is available via config or env.
  const tgToken =
    config.channels?.telegram?.token ??
    config.channels?.telegram?.accounts?.default?.token ??
    process.env.SIDECAR_TELEGRAM_TOKEN ??
    process.env.ELI_TELEGRAM_TOKEN ??
    "";
  if (tgToken && !registry.channels.has("telegram")) {
    try {
      const { telegramPlugin } = require("../plugins/telegram.js");
      registry.registerChannel(telegramPlugin);
      log.info("builtin telegram plugin registered");
    } catch (err: any) {
      log.error("failed to load builtin telegram plugin", { err: err.message });
    }
  }
}

/**
 * Discover and load all plugins listed in the config.
 * Each plugin's `register()` is called with a SidecarPluginApi instance.
 *
 * Under jiti (start.cjs), require() is kept under jiti transforms so
 * TypeScript-first plugins published from `node_modules` still load on
 * modern Node.js runtimes that reject native type stripping there.
 */
export async function loadPlugins(config: SidecarConfig): Promise<void> {
  loadBuiltinPlugins(config);
  const pluginRuntime = buildPluginRuntime(config);

  for (const pluginName of config.plugins) {
    try {
      const mod = require(pluginName);
      const plugin: OpenClawPluginDefinition = mod.default ?? mod;

      if (typeof plugin.register !== "function") {
        log.error("plugin has no register(), skipping", { plugin: pluginName });
        continue;
      }

      // Pre-registration: inject runtime via lifecycle hook or legacy fallback.
      if (plugin.lifecycle?.initRuntime) {
        try {
          plugin.lifecycle.initRuntime(pluginRuntime, pluginName);
        } catch (e: any) {
          log.warn("lifecycle.initRuntime failed", { plugin: pluginName, err: e.message });
        }
      } else {
        // Legacy fallback: try Lark-specific runtime injection.
        try {
          const pluginDir = require("path").dirname(require.resolve(pluginName));
          const { LarkClient } = require(pluginDir + "/src/core/lark-client.js");
          LarkClient.setRuntime(pluginRuntime);

          // Override the static getter permanently so even if a new class copy
          // appears, calls to LarkClient.runtime still work.
          Object.defineProperty(LarkClient, 'runtime', {
            get: () => pluginRuntime,
            configurable: true,
          });

          // Also monkey-patch the require cache entry so any future require()
          // of lark-client.js returns a patched LarkClient.
          const lcKey = Object.keys(require.cache).find(
            k => k.includes("lark-client") && k.endsWith(".js")
          );
          if (lcKey) {
            const origExports = require.cache[lcKey]!.exports;
            Object.defineProperty(origExports.LarkClient, 'runtime', {
              get: () => pluginRuntime,
              configurable: true,
            });
          }
        } catch (e: any) {
          log.debug("setRuntime skipped", { plugin: pluginName, err: e.message });
        }
      }

      const api = new SidecarPluginApi(plugin.id ?? pluginName, config, pluginRuntime);
      plugin.register(api);
      log.info("loaded plugin", { plugin: plugin.id ?? pluginName });

      // Discover SKILL.md files from the plugin's skills/ directory.
      installPluginSkills(pluginName);
    } catch (err) {
      log.error("failed to load plugin", { plugin: pluginName, err: String(err) });
    }
  }
}

/** Tracks SKILL.md files written by the sidecar for cleanup on shutdown. */
const writtenSkillDirs: string[] = [];

/**
 * Copy a plugin's SKILL.md files to .agents/skills/ so any agent discovers
 * them via the standard skills protocol (discover_skills).
 *
 * This is the standard integration path — no custom endpoints or registration.
 * Agent reads SKILL.md → sees tool names → calls via sidecar bridge tool.
 */
function installPluginSkills(pluginName: string): void {
  const path = require("path");
  const fs = require("fs");

  let pluginDir: string;
  try {
    pluginDir = path.dirname(require.resolve(pluginName));
  } catch {
    return;
  }

  const srcSkillsDir = path.join(pluginDir, "skills");
  if (!fs.existsSync(srcSkillsDir)) return;

  // Write to SIDECAR_SKILLS_DIR if set (eli passes workspace path),
  // otherwise fall back to cwd.
  const skillsRoot = process.env.SIDECAR_SKILLS_DIR || process.cwd();
  const destSkillsDir = path.join(skillsRoot, ".agents", "skills");
  fs.mkdirSync(destSkillsDir, { recursive: true });

  for (const entry of fs.readdirSync(srcSkillsDir, { withFileTypes: true }) as any[]) {
    if (!entry.isDirectory()) continue;
    const srcSkillMd = path.join(srcSkillsDir, entry.name, "SKILL.md");
    if (!fs.existsSync(srcSkillMd)) continue;

    const destDir = path.join(destSkillsDir, entry.name);
    const destFile = path.join(destDir, "SKILL.md");

    // Don't overwrite user's own SKILL.md files.
    if (fs.existsSync(destFile)) {
      skillsLog.debug("skipped (already exists)", { skill: entry.name });
      continue;
    }

    try {
      fs.mkdirSync(destDir, { recursive: true });

      // Read source and inject tool-calling instruction after frontmatter.
      let content: string = fs.readFileSync(srcSkillMd, "utf-8");
      const toolCallHint = `\n> **Tool calling:** Use \`sidecar(tool="<tool_name>", params={...})\` to call tools in this skill.\n`;
      const fmEnd = content.indexOf("\n---\n");
      if (fmEnd !== -1) {
        const insertAt = fmEnd + 5; // after "---\n"
        content = content.slice(0, insertAt) + toolCallHint + content.slice(insertAt);
      } else {
        content = toolCallHint + content;
      }
      fs.writeFileSync(destFile, content);

      writtenSkillDirs.push(destDir);
      skillsLog.info("installed", { skill: entry.name });
    } catch (e: any) {
      skillsLog.warn("failed to install", { skill: entry.name, err: e.message });
    }
  }

  // Also generate SKILL.md files for tool groups that don't have one.
  generateMissingSkills(destSkillsDir, srcSkillsDir);
}

/**
 * For tools not covered by any plugin SKILL.md, auto-generate a minimal one.
 */
function generateMissingSkills(destSkillsDir: string, srcSkillsDir: string): void {
  const path = require("path");
  const fs = require("fs");

  // Find which tools are covered by existing SKILL.md files.
  const coveredPrefixes = new Set<string>();
  if (fs.existsSync(srcSkillsDir)) {
    for (const entry of fs.readdirSync(srcSkillsDir, { withFileTypes: true }) as any[]) {
      if (entry.isDirectory()) coveredPrefixes.add(entry.name.replace(/-/g, "_"));
    }
  }

  // Group uncovered tools.
  const uncovered = new Map<string, Array<{ name: string; description: string }>>();
  for (const t of registry.tools.values()) {
    const isCovered = Array.from(coveredPrefixes).some((p) => t.name.startsWith(p));
    if (isCovered) continue;

    const parts = t.name.split("_");
    const group = parts.length >= 3 ? `${parts[0]}-${parts[1]}` : parts[0];
    if (!uncovered.has(group)) uncovered.set(group, []);
    uncovered.get(group)!.push({ name: t.name, description: t.description });
  }

  for (const [groupName, tools] of uncovered) {
    const destDir = path.join(destSkillsDir, groupName);
    const destFile = path.join(destDir, "SKILL.md");
    if (fs.existsSync(destFile)) continue;

    const toolNames = tools.map((t) => t.name).join(", ");
    let body = `---\nname: ${groupName}\ndescription: "${tools.length} tools: ${toolNames}"\n---\n\n`;
    body += `Call tools via: sidecar(tool="<name>", params={...})\n\n`;
    for (const t of tools) {
      body += `## ${t.name}\n${t.description || "(no description)"}\n\n`;
    }

    try {
      fs.mkdirSync(destDir, { recursive: true });
      fs.writeFileSync(destFile, body);
      writtenSkillDirs.push(destDir);
      skillsLog.debug("generated skill", { group: groupName });
    } catch {}
  }
}

/**
 * Skill files are project-local content and should persist across gateway
 * restarts — otherwise any project-committed override at .agents/skills/*
 * would only survive until the first shutdown of the same session. The
 * install logic in installPluginSkills already skips pre-existing files,
 * so leaving installed files in place is harmless on subsequent startups.
 */
export function cleanupInstalledSkills(): void {
  writtenSkillDirs.length = 0;
}

// ---------------------------------------------------------------------------
// Abort controllers per channel account (for graceful shutdown)
// ---------------------------------------------------------------------------

const abortControllers = new Map<string, AbortController>();

// ---------------------------------------------------------------------------
// Channel lifecycle
// ---------------------------------------------------------------------------

/**
 * Build the gateway context object that OpenClaw channel plugins expect.
 * This mimics the ctx object from openclaw's gateway runtime.
 */
function buildGatewayContext(
  channelId: string,
  accountId: string,
  config: SidecarConfig,
  onMessage: (envelope: InboundEnvelope) => Promise<void>,
) {
  const channelConfig = config.channels[channelId] ?? { accounts: {} };

  // Build cfg in the shape OpenClaw plugins expect:
  //   cfg.channels.feishu = { appId, appSecret, accounts: { default: {...} }, ... }
  const cfg = {
    channels: {
      [channelId]: channelConfig,
    },
  };

  const ac = new AbortController();
  abortControllers.set(`${channelId}:${accountId}`, ac);

  return {
    accountId,
    cfg,
    config: channelConfig.accounts?.[accountId] ?? channelConfig,

    // Logger scoped to this channel account.
    log: {
      ...pluginLogger(`${channelId}.${accountId}`),
    },

    // Status callback — log and ignore.
    setStatus: (status: any) => {
      logger(`${channelId}.${accountId}`).debug("status", { status: JSON.stringify(status) });
    },

    // Abort signal for graceful shutdown.
    abortSignal: ac.signal,

    // Minimal runtime — plugins use runtime.log, runtime.error, and
    // runtime.config.loadConfig() for the message dispatch pipeline.
    runtime: buildPluginRuntime(config),

    // The onMessage callback for inbound messages.
    onMessage,
  };
}

/**
 * Start all registered channel gateways.
 */
export async function startChannels(config: SidecarConfig): Promise<void> {
  for (const [channelId, plugin] of registry.channels) {
    if (!plugin.gateway) {
      log.debug("channel has no gateway, outbound only", { channel: channelId });
      continue;
    }

    const channelConfig = config.channels[channelId] ?? { accounts: {} };
    let accountIds: string[];

    try {
      accountIds = await plugin.config.listAccountIds({
        channels: { [channelId]: channelConfig },
      });
    } catch {
      accountIds = Object.keys(channelConfig.accounts ?? {});
      if (accountIds.length === 0) {
        accountIds = ["default"];
      }
    }

    // If no real accounts exist and the plugin supports QR login, trigger it.
    const hasRealAccounts =
      accountIds.length > 0 &&
      !(accountIds.length === 1 && accountIds[0] === "default");
    if (!hasRealAccounts && plugin.gateway?.loginWithQrStart) {
      log.info("no accounts configured, starting QR login", { channel: channelId });
      try {
        const startResult = await (plugin.gateway as any).loginWithQrStart({});
        const qrUrl = (startResult as any).qrDataUrl;
        const sessionKey = (startResult as any).sessionKey;
        if (qrUrl) {
          console.log(`\n使用微信扫描以下二维码，以完成连接：\n`);  // UX output
          try {
            const qrterm = require("qrcode-terminal");
            qrterm.generate(qrUrl, { small: true }, (qr: string) => {
              console.log(qr);  // UX output
              console.log(`如果二维码未能成功展示，请用浏览器打开以下链接扫码：`);  // UX output
              console.log(qrUrl);  // UX output
            });
          } catch {
            console.log(`请用浏览器打开以下链接扫码：`);  // UX output
            console.log(qrUrl);  // UX output
          }
          console.log(`\n等待扫码...\n`);  // UX output
          const waitResult = await (plugin.gateway as any).loginWithQrWait({
            sessionKey,
            timeoutMs: 300_000,
          });
          if (waitResult.connected) {
            log.info("QR login succeeded", { channel: channelId });
            // Re-list accounts after login.
            try {
              accountIds = await plugin.config.listAccountIds({
                channels: { [channelId]: channelConfig },
              });
            } catch {
              accountIds = [];
            }
          } else {
            log.error("QR login failed", { channel: channelId, message: waitResult.message });
          }
        } else {
          log.error("QR login start failed", { channel: channelId, message: startResult.message });
        }
      } catch (err) {
        log.error("QR login error", { channel: channelId, err: String(err) });
      }
    }

    for (const accountId of accountIds) {
      log.info("starting channel", { channel: channelId, account: accountId });

      const onMessage = async (envelope: InboundEnvelope) => {
        envelope.channel = envelope.channel || channelId;
        envelope.accountId = envelope.accountId || accountId;
        await sendToEli(envelope);
      };

      const ctx = buildGatewayContext(channelId, accountId, config, onMessage);

      // Resolve account for plugins that need ctx.account (e.g. weixin).
      try {
        const account = plugin.config.resolveAccount(
          ctx.cfg,
          accountId,
        );
        (ctx as any).account = account;
      } catch {
        // Plugin doesn't support resolveAccount or account not found — skip.
      }

      const startFn = plugin.gateway.startAccount ?? plugin.gateway.start;
      if (!startFn) {
        log.error("channel gateway has no start/startAccount", { channel: channelId });
        continue;
      }
      // Fire-and-forget: gateways run in the background (some block forever).
      void startFn.call(plugin.gateway, ctx)
        .then(() => log.debug("channel gateway returned", { channel: channelId, account: accountId }))
        .catch((err: any) => log.error("failed to start channel", { channel: channelId, account: accountId, err: String(err) }));
    }
  }
}

/**
 * Stop all registered channel gateways.
 */
export async function stopChannels(config: SidecarConfig): Promise<void> {
  const allowRemoteShutdown = config.allow_remote_shutdown === true;

  // Signal abort to all running gateways.
  for (const [key, ac] of abortControllers) {
    ac.abort();
    abortControllers.delete(key);
  }

  for (const [channelId, plugin] of registry.channels) {
    const stopFn = plugin.gateway?.stopAccount ?? plugin.gateway?.stop;
    if (!stopFn) continue;

    const channelConfig = config.channels[channelId] ?? { accounts: {} };
    let accountIds: string[];

    try {
      accountIds = await plugin.config.listAccountIds({
        channels: { [channelId]: channelConfig },
      });
    } catch {
      accountIds = Object.keys(channelConfig.accounts ?? {});
    }

    for (const accountId of accountIds) {
      if (!allowRemoteShutdown) {
        log.debug("skipping remote stop hook (allow_remote_shutdown=false)", { channel: channelId });
        continue;
      }

      try {
        // Timeout per-account stop to prevent one hung plugin from blocking shutdown.
        await Promise.race([
          Promise.resolve(
            stopFn.call(plugin.gateway, {
              accountId,
              cfg: { channels: { [channelId]: channelConfig } },
            } as any),
          ),
          new Promise((_, reject) =>
            setTimeout(() => reject(new Error("stop timeout")), 5000),
          ),
        ]);
      } catch (err: any) {
        log.error("failed to stop channel", { channel: channelId, account: accountId, err: String(err) });
      }
    }
  }
}
