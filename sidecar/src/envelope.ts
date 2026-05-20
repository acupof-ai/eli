import { withBridgeContract } from "./contract.js";
import type {
  EliBridgeMediaItem,
  EliBridgeContext,
  EliChannelMessage,
  InboundEnvelope,
  InboundMediaItem,
} from "./types.js";

function mediaToPayload(item: InboundMediaItem): EliBridgeMediaItem {
  const payload: EliBridgeMediaItem = { media_type: item.media_type };
  if (item.mime_type) payload.mime_type = item.mime_type;
  if (item.filename) payload.filename = item.filename;
  if (item.path) payload.path = item.path;
  if (item.data_base64) payload.data_base64 = item.data_base64;
  return payload;
}

/**
 * Convert an OpenClaw-style inbound envelope to eli's ChannelMessage format
 * for POSTing to the webhook channel.
 */
export function envelopeToEliMessage(env: InboundEnvelope): EliChannelMessage {
  const chatId = env.chatId ?? env.senderId;
  const sessionId = `${env.channel}:${env.accountId}:${chatId}`;

  // Merge plugin-supplied context (e.g. feishu-cli writes message_id,
  // msg_type, etc. so the LLM's prompt sees them) UNDERNEATH the fixed
  // fields so a misbehaving plugin can never shadow source_channel /
  // account_id / chat_type.
  const extra = (env.context && typeof env.context === "object" ? env.context : {}) as Record<string, any>;
  const context: EliBridgeContext = {
    ...extra,
    source_channel: env.channel,
    account_id: env.accountId,
    sender_id: env.senderId,
    sender_name: env.senderName ?? "",
    chat_type: env.chatType,
    group_label: env.groupLabel ?? "",
    reply_to_id: env.replyToId ?? "",
    channel_target: env.channel_target ?? (env as any).feishu_to ?? "",
  };

  // Backward-compatible fallback for channels that still only provide local file paths.
  if (env.media_paths && env.media_paths.length > 0) {
    context.media_paths = env.media_paths;
    context.media_types = env.media_types ?? [];
  }

  return withBridgeContract({
    session_id: sessionId,
    channel: "webhook",
    content: typeof env.text === "string" ? env.text : JSON.stringify(env.text),
    chat_id: chatId,
    is_active: true,
    kind: "normal",
    media: env.media?.map(mediaToPayload) ?? [],
    context,
    output_channel: "webhook",
  });
}

/**
 * Extract routing info from an eli outbound message to determine which
 * channel plugin + account to send through.
 */
export function parseOutboundTarget(msg: EliChannelMessage): {
  sourceChannel: string;
  accountId: string;
  chatId: string;
  chatType: "direct" | "group";
} {
  return {
    sourceChannel: msg.context?.source_channel ?? "",
    accountId: msg.context?.account_id ?? "default",
    chatId: msg.chat_id,
    chatType: (msg.context?.chat_type as "direct" | "group") ?? "direct",
  };
}
