import assert from 'node:assert/strict';
import {
  buildCompletionContext,
  isCompletableDocument,
  isCompletableLanguage,
  trimContext,
  MAX_PREFIX_CHARS,
  MAX_SUFFIX_CHARS,
  MAX_DOCUMENT_CHARS,
} from '../../src/cue/context';

// The context is split at the cursor, and the split is exact.
const text = 'fn main() {\n    println!("hi");\n}';
const offset = text.indexOf('println');
const context = buildCompletionContext(text, offset);
assert.equal(context.prefix, 'fn main() {\n    ');
assert.equal(context.suffix, 'println!("hi");\n}');

// An offset outside the document is clamped, never out of range.
assert.deepEqual(buildCompletionContext('abc', 99), { prefix: 'abc', suffix: '' });
assert.deepEqual(buildCompletionContext('abc', -5), { prefix: '', suffix: 'abc' });

// The prefix keeps its TAIL and the suffix keeps its HEAD: the code just
// above the cursor predicts it, and the code just below constrains it.
const trimmed = trimContext({
  prefix: 'a'.repeat(MAX_PREFIX_CHARS + 100),
  suffix: 'b'.repeat(MAX_SUFFIX_CHARS + 100),
});
assert.equal(trimmed.prefix.length, MAX_PREFIX_CHARS);
assert.equal(trimmed.suffix.length, MAX_SUFFIX_CHARS);
assert.ok(trimmed.prefix.endsWith('a'));
assert.ok(trimmed.suffix.startsWith('b'));

// A short document is left whole.
const short = trimContext({ prefix: 'x', suffix: 'y' });
assert.deepEqual(short, { prefix: 'x', suffix: 'y' });

// The cut lands on a CHARACTER boundary: a half emoji would reach the model
// as a replacement character, and the model would answer around a character
// that is not in the file.
const emoji = '😀'; // one emoji, two UTF-16 units
const pairPrefix = trimContext({
  prefix: 'a'.repeat(MAX_PREFIX_CHARS - 1) + emoji,
  suffix: '',
});
assert.ok(
  !pairPrefix.prefix.includes('�'),
  'the prefix must not carry half a surrogate pair',
);
assert.ok(pairPrefix.prefix.endsWith(emoji), 'the whole pair is kept');

const pairSuffix = trimContext({
  prefix: '',
  suffix: emoji + 'b'.repeat(MAX_SUFFIX_CHARS - 1),
});
assert.ok(
  !pairSuffix.suffix.includes('�'),
  'the suffix must not carry half a surrogate pair',
);
assert.ok(pairSuffix.suffix.startsWith(emoji), 'the whole pair is kept');

// A document too large to complete is refused outright — the model would be
// guessing at a file it cannot see.
const huge = buildCompletionContext('z'.repeat(MAX_DOCUMENT_CHARS + 1), 10);
assert.deepEqual(huge, { prefix: '', suffix: '' });

// Data files, config files and prose are not completed: a suggestion there
// is meaningless and still spends the user's quota.
assert.equal(isCompletableLanguage('json'), false);
assert.equal(isCompletableLanguage('markdown'), false);
assert.equal(isCompletableLanguage('yaml'), false);
assert.equal(isCompletableLanguage('rust'), true);
assert.equal(isCompletableLanguage('typescript'), true);
assert.equal(isCompletableLanguage('python'), true);

// A document needs a language worth completing, a real file, and some text.
assert.equal(isCompletableDocument('rust', 'file', 'fn main() {}'), true);
assert.equal(isCompletableDocument('rust', 'untitled', 'fn main() {}'), false);
assert.equal(isCompletableDocument('json', 'file', '{}'), false);
assert.equal(isCompletableDocument('rust', 'file', '   \n  '), false);

console.log('[cue-test] context rules hold');
