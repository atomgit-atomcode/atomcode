import test from 'node:test';
import assert from 'node:assert/strict';
import { isInternalHistoryUserMessage, sessionMessagesToMarkdownLines } from './historyMessages.ts';
import type { SessionMessage } from '../api.ts';

test('isInternalHistoryUserMessage hides synthetic and legacy internal users', () => {
  assert.equal(isInternalHistoryUserMessage('real prompt'), false);
  assert.equal(isInternalHistoryUserMessage('real prompt', true), true);
  assert.equal(
    isInternalHistoryUserMessage('You made code edits but have not verified them. Run cargo check.'),
    true,
  );
  assert.equal(
    isInternalHistoryUserMessage('Output limit hit when running pytest; how do I debug it?'),
    false,
  );
});

test('sessionMessagesToMarkdownLines skips internal user messages', () => {
  const messages: SessionMessage[] = [
    { role: 'user', content: 'real prompt' },
    { role: 'user', content: 'You made code edits but have not verified them.', synthetic: true },
    { role: 'user', content: '[Auto-read from error: src/main.rs]\nfn main() {}' },
    { role: 'assistant', content: 'reply' },
  ];

  const markdown = sessionMessagesToMarkdownLines(messages, 'Session').join('\n');

  assert.match(markdown, /## User\n\nreal prompt/);
  assert.match(markdown, /## Assistant\n\nreply/);
  assert.doesNotMatch(markdown, /not verified/);
  assert.doesNotMatch(markdown, /Auto-read/);
});
