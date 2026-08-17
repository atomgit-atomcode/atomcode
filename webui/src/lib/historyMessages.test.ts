import test from 'node:test';
import assert from 'node:assert/strict';
import {
  isInternalHistoryUserMessage,
  isUserInterruptionMessage,
  sessionMessagesToMarkdownLines,
} from './historyMessages.ts';
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

test('isUserInterruptionMessage recognizes provenance and legacy snapshots', () => {
  assert.equal(isUserInterruptionMessage({
    role: 'user',
    content: 'hidden boundary',
    synthetic: true,
    internal_origin: 'atomcode.user_interruption',
  }), true);
  assert.equal(isUserInterruptionMessage({
    role: 'user',
    content: '[The previous response was interrupted by the user before completing. Reconsider.]',
    synthetic: true,
  }), true);
  assert.equal(isUserInterruptionMessage({
    role: 'user',
    content: 'I interrupted the task myself',
  }), false);
});

test('sessionMessagesToMarkdownLines skips verify cadence assistant messages', () => {
  const messages: SessionMessage[] = [
    { role: 'user', content: 'create f.txt' },
    { role: 'assistant', content: 'No verification is needed.', internal_origin: 'verify_cadence' },
    { role: 'assistant', content: 'I am AtomCode.' },
  ];

  const markdown = sessionMessagesToMarkdownLines(messages, 'Session').join('\n');

  assert.doesNotMatch(markdown, /No verification is needed/);
  assert.match(markdown, /I am AtomCode/);
});

test('sessionMessagesToMarkdownLines skips camel case verify cadence assistant messages', () => {
  const messages: SessionMessage[] = [
    { role: 'assistant', content: 'No verification is needed.', internalOrigin: 'verify_cadence' },
    { role: 'assistant', content: 'I am AtomCode.' },
  ];

  const markdown = sessionMessagesToMarkdownLines(messages, 'Session').join('\n');

  assert.doesNotMatch(markdown, /No verification is needed/);
  assert.match(markdown, /I am AtomCode/);
});

test('sessionMessagesToMarkdownLines keeps verify cadence assistants with tool calls', () => {
  const messages: SessionMessage[] = [
    {
      role: 'assistant',
      content: 'Running verification',
      internal_origin: 'verify_cadence',
      tool_calls: [{ id: 'b1', name: 'bash', arguments: '{"command":"true"}' }],
    },
  ];

  const markdown = sessionMessagesToMarkdownLines(messages, 'Session').join('\n');

  assert.match(markdown, /Running verification/);
  assert.match(markdown, /### Tool: bash/);
});
