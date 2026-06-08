// 入站 WeixinMessage -> { fromUserId, text, contextToken } | null
// 仅 PoC：只处理文本（message_type===1 且 item_list 中 type===1 的 text_item）。
export function parseInbound(msg) {
  if (!msg || msg.message_type !== 1) return null;
  const item = (msg.item_list || []).find(
    (i) => i.type === 1 && i.text_item && typeof i.text_item.text === 'string',
  );
  if (!item) return null;
  return {
    fromUserId: msg.from_user_id,
    text: item.text_item.text,
    contextToken: msg.context_token,
  };
}
