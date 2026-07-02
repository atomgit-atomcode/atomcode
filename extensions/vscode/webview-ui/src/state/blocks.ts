import { ChatMessage, MessageBlock } from './types';

export function blocksFromLegacyMessage(message: ChatMessage): MessageBlock[] {
  const blocks: MessageBlock[] = [];
  if (message.text) {
    blocks.push({ id: `${message.id}-text-0`, type: 'text', content: message.text });
  }
  for (const tool of message.toolCalls ?? []) {
    blocks.push({ id: `${message.id}-tool-${tool.id}`, type: 'tool', tool });
  }
  for (const artifact of message.artifacts ?? []) {
    blocks.push({ id: `${message.id}-artifact-${artifact.id}`, type: 'artifact', artifact });
  }
  if (message.permissionRequest) {
    blocks.push({ id: `${message.id}-permission-${message.permissionRequest.id}`, type: 'permission', request: message.permissionRequest });
  }
  return blocks;
}
