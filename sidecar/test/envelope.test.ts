/**
 * Envelope conversion edge case tests.
 */

import { describe, it, expect } from "bun:test";
import { envelopeToEliMessage, parseOutboundTarget } from "../src/envelope.js";
import { ELI_BRIDGE_CONTRACT_VERSION } from "../src/contract.js";
import type { EliChannelMessage, InboundEnvelope } from "../src/types.js";

describe("parseOutboundTarget", () => {
  it("returns empty/default when context is missing", () => {
    const msg: EliChannelMessage = {
      contract_version: ELI_BRIDGE_CONTRACT_VERSION,
      session_id: "ch:acc:chat",
      channel: "webhook",
      content: "test",
      chat_id: "chat",
      output_channel: "webhook",
    };
    const target = parseOutboundTarget(msg);
    expect(target.sourceChannel).toBe("");
    expect(target.accountId).toBe("default");
    expect(target.chatId).toBe("chat");
    expect(target.chatType).toBe("direct");
  });

  it("returns empty/default when context is empty object", () => {
    const msg: EliChannelMessage = {
      contract_version: ELI_BRIDGE_CONTRACT_VERSION,
      session_id: "ch:acc:chat",
      channel: "webhook",
      content: "test",
      chat_id: "chat",
      context: {},
      output_channel: "webhook",
    };
    const target = parseOutboundTarget(msg);
    expect(target.sourceChannel).toBe("");
    expect(target.accountId).toBe("default");
  });

  it("extracts all routing fields from context", () => {
    const msg: EliChannelMessage = {
      contract_version: ELI_BRIDGE_CONTRACT_VERSION,
      session_id: "lark:bot1:group_1",
      channel: "webhook",
      content: "resp",
      chat_id: "group_1",
      context: {
        source_channel: "lark",
        account_id: "bot1",
        chat_type: "group",
      },
      output_channel: "webhook",
    };
    const target = parseOutboundTarget(msg);
    expect(target.sourceChannel).toBe("lark");
    expect(target.accountId).toBe("bot1");
    expect(target.chatId).toBe("group_1");
    expect(target.chatType).toBe("group");
  });
});

describe("envelopeToEliMessage", () => {
  it("handles minimal envelope with only required fields", () => {
    const env: InboundEnvelope = {
      channel: "test",
      accountId: "default",
      senderId: "u1",
      chatType: "direct",
      text: "hi",
    };
    const msg = envelopeToEliMessage(env);
    expect(msg.contract_version).toBe(ELI_BRIDGE_CONTRACT_VERSION);
    expect(msg.session_id).toBe("test:default:u1");
    expect(msg.channel).toBe("webhook");
    expect(msg.content).toBe("hi");
    expect(msg.chat_id).toBe("u1"); // fallback to senderId
    expect(msg.context.sender_name).toBe("");
    expect(msg.context.group_label).toBe("");
    expect(msg.context.reply_to_id).toBe("");
    expect(msg.context.channel_target).toBe("");
  });

  it("uses chatId over senderId when both present", () => {
    const env: InboundEnvelope = {
      channel: "test",
      accountId: "default",
      senderId: "u1",
      chatId: "group_99",
      chatType: "group",
      text: "hello group",
    };
    const msg = envelopeToEliMessage(env);
    expect(msg.session_id).toBe("test:default:group_99");
    expect(msg.chat_id).toBe("group_99");
  });

  it("stringifies non-string text", () => {
    const env: InboundEnvelope = {
      channel: "test",
      accountId: "default",
      senderId: "u1",
      chatType: "direct",
      text: { type: "rich", content: "hello" } as any,
    };
    const msg = envelopeToEliMessage(env);
    expect(msg.content).toBe(JSON.stringify({ type: "rich", content: "hello" }));
  });

  it("includes media_paths in context when present", () => {
    const env: InboundEnvelope = {
      channel: "test",
      accountId: "default",
      senderId: "u1",
      chatType: "direct",
      text: "with media",
      media_paths: ["/tmp/img.png"],
      media_types: ["image"],
    };
    const msg = envelopeToEliMessage(env);
    expect(msg.context.media_paths).toEqual(["/tmp/img.png"]);
    expect(msg.context.media_types).toEqual(["image"]);
  });

  it("maps media items to payload format", () => {
    const env: InboundEnvelope = {
      channel: "test",
      accountId: "default",
      senderId: "u1",
      chatType: "direct",
      text: "media test",
      media: [
        {
          media_type: "image",
          mime_type: "image/png",
          filename: "photo.png",
          path: "/tmp/photo.png",
        },
      ],
    };
    const msg = envelopeToEliMessage(env);
    expect(msg.media).toHaveLength(1);
    expect(msg.media![0].media_type).toBe("image");
    expect(msg.media![0].path).toBe("/tmp/photo.png");
    expect(msg.media![0].filename).toBe("photo.png");
  });

  it("produces empty media array when no media provided", () => {
    const env: InboundEnvelope = {
      channel: "test",
      accountId: "default",
      senderId: "u1",
      chatType: "direct",
      text: "no media",
    };
    const msg = envelopeToEliMessage(env);
    expect(msg.media).toEqual([]);
  });

  it("merges plugin-supplied env.context under fixed routing fields", () => {
    // Plugins (e.g. feishu-cli) stash domain metadata on env.context so the
    // LLM's prompt context line carries it through. Routing fields the
    // bridge depends on must still win on conflict.
    const env: InboundEnvelope = {
      channel: "feishu",
      accountId: "default",
      senderId: "ou_u1",
      chatType: "direct",
      chatId: "oc_c1",
      text: "hi",
      context: {
        message_id: "om_msg_42",
        msg_type: "text",
        custom_flag: "yes",
        // Adversarial: try to overwrite a fixed field — should be ignored.
        source_channel: "evil_channel",
      },
    };
    const msg = envelopeToEliMessage(env);
    expect(msg.context.message_id).toBe("om_msg_42");
    expect(msg.context.msg_type).toBe("text");
    expect(msg.context.custom_flag).toBe("yes");
    // Fixed routing field wins.
    expect(msg.context.source_channel).toBe("feishu");
  });
});
