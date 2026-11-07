import { test } from 'node:test';
import assert from 'node:assert/strict';
import { preservedHistoryPrefix } from './historyPaging.ts';
const cursor = {session:'project/session',before:34,digest:'same-prefix'};
const previous = [{role:'user',sourceSeq:2},{role:'assistant'},{role:'user',sourceSeq:70},{role:'assistant'}];
const incoming = [{role:'user',sourceSeq:70},{role:'assistant'},{role:'user',sourceSeq:74}];
test('a reconnect keeps fetched history while replacing the authoritative tail', () => {
  assert.deepEqual(preservedHistoryPrefix(previous,incoming,cursor,{...cursor}),previous.slice(0,2));
});
test('changed prefixes, sessions and missing native identities cannot retain stale pages', () => {
  for (const changed of [{...cursor,digest:'changed'},{...cursor,session:'other/session'},{...cursor,before:18}]) {
    assert.deepEqual(preservedHistoryPrefix(previous,incoming,cursor,changed),[]);
  }
  assert.deepEqual(preservedHistoryPrefix(previous,[{role:'user'}],cursor,cursor),[]);
  assert.deepEqual(preservedHistoryPrefix(previous,incoming,undefined,cursor),[]);
});
