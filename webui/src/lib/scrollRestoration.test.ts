import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createScrollRestoration } from './scrollRestoration.ts';

function fixture(follow = true) {
  const viewport = { scrollTop: follow ? 1500 : 420, scrollHeight: 2000, clientHeight: 500 };
  let session = 'session-a';
  let following = follow;
  let next = 0;
  const frames = new Map<number, () => void>();
  const scroll = createScrollRestoration({
    viewport: () => viewport, session: () => session, following: () => following,
    setFollowing: v => { following = v; },
    schedule: run => { frames.set(++next, run); return next; }, cancel: id => { frames.delete(id); },
  });
  return { viewport, scroll, frames,
    follows: () => following, setFollowing: (v: boolean) => { following = v; },
    switchSession: () => { session = 'session-b'; },
    flush: () => { for (let i = 0; frames.size && i < 10; i++) { const pending = [...frames.values()]; frames.clear(); pending.forEach(run => run()); } },
  };
}

test('resuming a backgrounded task follows new output instead of accepting the browser top reset', () => {
  const f = fixture();
  f.scroll.suspend();
  f.viewport.scrollTop = 0;
  f.viewport.scrollHeight = 3500;
  assert.equal(f.scroll.onScroll(), null);
  f.scroll.layout();
  assert.equal(f.viewport.scrollTop, 0, 'do not scroll a hidden page');
  f.scroll.resume();
  f.flush();
  assert.equal(f.viewport.scrollTop, 3000);
  assert.equal(f.follows(), true);
  assert.equal(f.scroll.onScroll(), true);
});

test('a reader in history keeps their place across backgrounding and later content changes', () => {
  const f = fixture(false);
  f.scroll.suspend();
  f.viewport.scrollTop = 0;
  f.viewport.scrollHeight = 5000;
  f.scroll.resume();
  f.flush();
  assert.equal(f.viewport.scrollTop, 420);
  assert.equal(f.follows(), false);
  f.viewport.scrollHeight += 200;
  f.scroll.layout();
  assert.equal(f.viewport.scrollTop, 420);
});

test('zero-height viewport during browser restoration does not discard saved scroll intent', () => {
  const f = fixture();
  f.scroll.suspend();
  f.viewport.clientHeight = 0;
  f.viewport.scrollTop = 0;
  f.scroll.resume();
  f.flush();
  assert.equal(f.scroll.onScroll(), null);
  f.viewport.clientHeight = 600;
  f.viewport.scrollHeight = 4000;
  f.scroll.layout();
  f.flush();
  assert.equal(f.viewport.scrollTop, 3400);
});

test('manual scrolling wins over queued resume frames', () => {
  const f = fixture();
  f.scroll.suspend();
  f.scroll.resume();
  f.scroll.interact();
  f.viewport.scrollTop = 300;
  f.setFollowing(f.scroll.onScroll()!);
  f.flush();
  f.scroll.layout();
  assert.equal(f.viewport.scrollTop, 300);
  assert.equal(f.follows(), false);
});

test('old session positions and disposed callbacks cannot move a replacement view', () => {
  const f = fixture(false);
  f.scroll.suspend();
  f.switchSession();
  f.viewport.scrollTop = 200;
  f.scroll.resume();
  f.flush();
  assert.equal(f.viewport.scrollTop, 200);
  f.scroll.resume();
  f.scroll.dispose();
  f.viewport.scrollTop = 100;
  f.flush();
  assert.equal(f.viewport.scrollTop, 100);
});


test('a session switched while hidden uses its own follow intent rather than the old position', () => {
  const f = fixture(false);
  f.scroll.suspend();
  f.switchSession();
  f.setFollowing(true);
  f.viewport.scrollTop = 0;
  f.scroll.resume();
  f.flush();
  assert.equal(f.viewport.scrollTop, 1500);
});

test('a small upward scroll releases following until the user reaches the bottom', () => {
  const f = fixture();
  f.scroll.layout();
  f.viewport.scrollTop -= 25;
  f.setFollowing(f.scroll.onScroll()!);
  assert.equal(f.follows(), false);
  f.viewport.scrollHeight += 100;
  f.scroll.layout();
  assert.equal(f.viewport.scrollTop, 1475);
  f.viewport.scrollTop = 1550;
  f.setFollowing(f.scroll.onScroll()!);
  assert.equal(f.follows(), false, 'being within the former 80px slack does not resume following');
  f.viewport.scrollTop = 1600;
  f.setFollowing(f.scroll.onScroll()!);
  assert.equal(f.follows(), true);
  f.viewport.scrollHeight += 100;
  f.scroll.layout();
  assert.equal(f.viewport.scrollTop, 1700);
});

test('layout growth and a no-op scroll do not cancel follow intent', () => {
  const f = fixture();
  f.scroll.layout();
  f.viewport.scrollHeight += 100;
  f.setFollowing(f.scroll.onScroll()!);
  assert.equal(f.follows(), true);
  f.scroll.layout();
  assert.equal(f.viewport.scrollTop, 1600);
});

test('layout shrink at the bottom does not look like upward reading', () => {
  const f = fixture();
  f.scroll.layout();
  f.viewport.scrollHeight -= 100;
  f.viewport.scrollTop -= 100;
  f.setFollowing(f.scroll.onScroll()!);
  assert.equal(f.follows(), true);
});

test('layout measures reply space before following and tracks the corrected position', () => {
  const viewport = { scrollTop: 0, scrollHeight: 600, clientHeight: 400 };
  const scroll = createScrollRestoration({
    viewport: () => viewport, session: () => 'a', following: () => true,
    setFollowing() {}, schedule: () => 0, cancel() {},
    prepareLayout: () => { viewport.scrollHeight = 1000; },
  });
  scroll.layout();
  assert.equal(viewport.scrollTop, 600);
  assert.equal(scroll.onScroll(), true);
  viewport.clientHeight = 500;
  scroll.layout();
  assert.equal(viewport.scrollTop, 500);
  assert.equal(scroll.onScroll(), true, 'viewport resizing does not look like scrolling up');
});

test('an oversized new prompt can open at its beginning and release following', () => {
  const viewport = { scrollTop: 0, scrollHeight: 2000, clientHeight: 400 };
  let following = true;
  const scroll = createScrollRestoration({
    viewport: () => viewport, session: () => 'a', following: () => following,
    setFollowing: value => { following = value; }, schedule: () => 0, cancel() {},
    followingTop: () => { following = false; return 800; },
  });
  scroll.layout();
  assert.equal(viewport.scrollTop, 800);
  viewport.scrollHeight += 100;
  scroll.layout();
  assert.equal(viewport.scrollTop, 800, 'streaming keeps the long prompt readable');
});
