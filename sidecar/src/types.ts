// ---------------------------------------------------------------------------
// OpenClaw plugin interface subset — just enough to host real plugins.
// ---------------------------------------------------------------------------

/** Metadata adapter for a channel plugin. */
export interface ChannelMeta {
  id: string;
  label: string;
  selectionLabel?: string;
  docsPath?: string;
  blurb?: string;
  aliases?: string[];
}

/** Config adapter — resolves account credentials. */
export interface ChannelConfig {
  listAccountIds(cfg: any): string[] | Promise<string[]>;
  resolveAccount(cfg: any, accountId: string): any;
}

/** Capabilities declaration. */
export interface ChannelCapabilities {
  chatTypes: Array<"direct" | "group">;
  [key: string]: any;
}

/** Outbound adapter — sends messages TO the platform. */
export interface ChannelOutbound {
  deliveryMode?: "direct" | "queued";
  sendText(params: OutboundTextParams): Promise<OutboundResult>;
  sendMedia?(params: OutboundMediaParams): Promise<OutboundResult>;
  [key: string]: any;
}

export interface OutboundTextParams {
  cfg: any;
  to: string;
  text: string;
  accountId: string;
  replyToId?: string;
  threadId?: string;
  /**
   * Discriminates "final" LLM replies (consume pending state, quote the
   * inbound) from "notice" tool-progress / status messages emitted mid-turn
   * (do not consume pending state, send as plain non-quoted text).
   * Defaults to "final" when omitted so existing channels keep working.
   */
  kind?: "final" | "notice";
  [key: string]: any;
}

export interface OutboundMediaParams {
  text?: string;
  mediaPath: string;
  mediaType: string;
  target: OutboundTarget;
  config: any;
  accountId: string;
}

export interface OutboundTarget {
  chatId: string;
  chatType: "direct" | "group";
  senderId?: string;
  [key: string]: any;
}

export interface OutboundResult {
  ok: boolean;
  error?: string;
  [key: string]: any;
}

/** Gateway adapter — receives messages FROM the platform. */
export interface ChannelGateway {
  start?(params: GatewayStartParams): Promise<void>;
  stop?(params: GatewayStopParams): Promise<void>;
  /** OpenClaw uses startAccount/stopAccount in some plugins. */
  startAccount?(params: GatewayStartParams): Promise<void>;
  stopAccount?(params: GatewayStopParams): Promise<void>;
  loginWithQrStart?(params: GatewayQrStartParams): Promise<any>;
  loginWithQrWait?(params: GatewayQrWaitParams): Promise<any>;
}

export interface GatewayStartParams {
  accountId: string;
  config: any;
  onMessage: (envelope: InboundEnvelope) => void | Promise<void>;
  [key: string]: any;
}

export interface GatewayStopParams {
  accountId: string;
  cfg?: any;
  config?: any;
  [key: string]: any;
}

export interface GatewayQrStartParams {
  accountId?: string;
  force?: boolean;
  [key: string]: any;
}

export interface GatewayQrWaitParams {
  sessionKey: string;
  accountId?: string;
  timeoutMs?: number;
  [key: string]: any;
}

/** The composable channel plugin object registered via api.registerChannel(). */
export interface ChannelPlugin {
  id?: string;
  meta: ChannelMeta;
  config: ChannelConfig;
  capabilities: ChannelCapabilities;
  outbound?: ChannelOutbound;
  gateway?: ChannelGateway;
  lifecycle?: ChannelLifecycleHooks;
  security?: any;
  groups?: any;
  mentions?: any;
  threading?: any;
  actions?: any;
}

// ---------------------------------------------------------------------------
// Channel lifecycle hooks — optional per-plugin capabilities.
// ---------------------------------------------------------------------------

/** Per-session context stored at inbound time, used for tool execution. */
export interface SessionContext {
  channel: string;
  messageId: string;
  chatId: string;
  channelTarget?: string;
  accountId: string;
  senderId: string;
  chatType: string;
  cfg: any;
}

export type ToolCallPhase = "before" | "after";

export interface ToolCallLifecycleEvent {
  phase: ToolCallPhase;
  toolName: string;
  params: any;
  session: SessionContext;
  description?: string;
  durationMs?: number;
  result?: unknown;
  error?: string;
}

/**
 * Optional hooks a channel plugin can provide to handle plugin-specific
 * concerns (runtime injection, typing indicators, tool auth context)
 * without hardcoding them in the sidecar core.
 */
export interface ChannelLifecycleHooks {
  /** Called before plugin.register() to inject runtime (e.g. LarkClient.setRuntime). */
  initRuntime?(pluginRuntime: any, pluginName: string): void;
  /** Called on inbound message — return typing state for cleanup. */
  onInboundMessage?(params: { cfg: any; messageId: string; accountId: string; sessionId: string }): Promise<any>;
  /** Called on outbound reply — clean up typing indicators etc. */
  onOutboundReply?(params: { cfg: any; typingState: any; accountId: string }): Promise<void>;
  /** Wrap tool execution with channel context (e.g. LarkTicket for OAuth). */
  wrapToolExecution?<T>(ctx: SessionContext, fn: () => Promise<T>): Promise<T>;
  /** Resolve outbound target from message context. Default: chatId. */
  resolveOutboundTarget?(context: Record<string, any>, chatId: string): string;
  /** Render a human-facing tool progress message for the channel, or null to suppress it. */
  renderToolCallText?(
    event: ToolCallLifecycleEvent,
  ): string | null | Promise<string | null>;
}

// ---------------------------------------------------------------------------
// Inbound envelope — normalized message from any channel.
// ---------------------------------------------------------------------------

export interface EliBridgeMediaItem {
  media_type: "image" | "file" | "audio" | "video" | "sticker" | "document";
  mime_type?: string;
  filename?: string;
  path?: string;
  data_base64?: string;
}

export interface InboundMediaItem extends EliBridgeMediaItem {}

export interface EliBridgeContext {
  source_channel?: string;
  account_id?: string;
  sender_id?: string;
  sender_name?: string;
  chat_type?: "direct" | "group" | "p2p" | string;
  group_label?: string;
  reply_to_id?: string;
  channel_target?: string;
  outbound_media?: EliBridgeMediaItem[];
  _eli_cleanup_only?: boolean;
  /**
   * Set by the eli `message.send` tool when the LLM dispatches a mid-turn
   * progress / acknowledgement message via dispatch_mid_turn. Channel
   * plugins should treat this as a notice (no inflight state consumption,
   * no quote-reply target, no reaction cleanup).
   */
  _eli_mid_turn?: boolean;
  [key: string]: any;
}

export interface InboundEnvelope {
  channel: string;
  accountId: string;
  senderId: string;
  senderName?: string;
  chatType: "direct" | "group";
  chatId?: string;
  groupLabel?: string;
  text: string;
  mediaPath?: string;
  /** Local file paths for media attachments (resolved by channel plugin). */
  media_paths?: string[];
  /** Media types corresponding to media_paths (image, file, audio, video, sticker). */
  media_types?: string[];
  /** Structured inbound media; prefer this over media_paths for first-turn fidelity. */
  media?: InboundMediaItem[];
  replyToId?: string;
  [key: string]: any;
}

// ---------------------------------------------------------------------------
// Tool definition — subset of OpenClaw tool registration.
// ---------------------------------------------------------------------------

export interface ToolDefinition {
  name: string;
  description: string;
  parameters: Record<string, any>;
  /** Plugin-declared group name. Falls back to heuristic if omitted. */
  group?: string;
  execute(id: string, params: any): Promise<ToolResult>;
}

export interface ToolResult {
  content: Array<{ type: string; text: string }>;
}

// ---------------------------------------------------------------------------
// Hook handler.
// ---------------------------------------------------------------------------

export interface HookHandler {
  (event: HookEvent): void | Promise<void>;
}

export interface HookEvent {
  type: string;
  action: string;
  sessionKey?: string;
  timestamp: Date;
  messages?: string[];
  context?: Record<string, any>;
}

export interface HookOptions {
  eventType?: string;
  [key: string]: any;
}

// ---------------------------------------------------------------------------
// Plugin definition — the default export of an OpenClaw plugin.
// ---------------------------------------------------------------------------

export interface OpenClawPluginDefinition {
  id: string;
  name?: string;
  description?: string;
  configSchema?: Record<string, any>;
  /** Pre-registration lifecycle hooks (e.g. runtime injection). */
  lifecycle?: ChannelLifecycleHooks;
  register(api: OpenClawPluginApi): void;
}

// ---------------------------------------------------------------------------
// Plugin API — the object passed to plugin.register().
// ---------------------------------------------------------------------------

export interface OpenClawPluginApi {
  registerChannel(opts: { plugin: ChannelPlugin }): void;
  registerTool(tool: ToolDefinition, options?: { optional?: boolean }): void;
  registerHook(handler: HookHandler, options?: HookOptions): void;
  registerCommand?(command: any): void;
  registerCli?(cli: any): void;
  registerGatewayMethod?(name: string, handler: any): void;
  registerGatewayRequestHandler?(path: string, handler: any): void;
  registerProviderAuthMethod?(...args: any[]): void;
  registerMemoryPromptSection?(builder: any): void;
  registerService?(service: any): void;
  logger: PluginLogger;
  config: any;
  runtime?: any;
}

export interface PluginLogger {
  info(...args: any[]): void;
  warn(...args: any[]): void;
  error(...args: any[]): void;
  debug(...args: any[]): void;
}

// ---------------------------------------------------------------------------
// Eli bridge message format.
// ---------------------------------------------------------------------------

export interface EliChannelMessage {
  contract_version: string;
  session_id: string;
  channel: string;
  content: string;
  chat_id: string;
  is_active: boolean;
  kind?: "normal" | "error" | "command";
  context: EliBridgeContext;
  output_channel: string;
  media?: EliBridgeMediaItem[];
}
