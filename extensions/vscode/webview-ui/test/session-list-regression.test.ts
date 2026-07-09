import assert from 'node:assert/strict';
import {
  filterSessionsForQuery,
} from '../src/lib/sessionGroups';
import type { SessionMeta } from '../src/state/types';

const NOW = 1_725_000_000_000;

function session(overrides: Partial<SessionMeta>): SessionMeta {
  return {
    id: 'session',
    name: 'Untitled',
    created_at: NOW,
    updated_at: NOW,
    ...overrides,
  };
}

function testSearchMatchesWorkingDirectorySessionIdAndProjectHash() {
  const sessions = [
    session({
      id: 'abc123',
      name: 'Weather lookup',
      project_hash: 'weather-hash',
      working_dir: '/Users/dev/work/weather-app',
    }),
    session({
      id: 'def456',
      name: 'Release notes',
      project_hash: 'release-hash',
      working_dir: '/Users/dev/work/release-tool',
    }),
  ];

  assert.deepEqual(filterSessionsForQuery(sessions, 'weather-app').map((s) => s.id), ['abc123']);
  assert.deepEqual(filterSessionsForQuery(sessions, 'def').map((s) => s.id), ['def456']);
  assert.deepEqual(filterSessionsForQuery(sessions, 'release-hash').map((s) => s.id), ['def456']);
}

testSearchMatchesWorkingDirectorySessionIdAndProjectHash();
