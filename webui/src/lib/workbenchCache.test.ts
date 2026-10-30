import test from "node:test";
import assert from "node:assert/strict";
import { WorkbenchCache } from "./workbenchCache.ts";
test("late save results update a remounted session view without losing newer typing", async () => {
  type Draft = { text: string; original: string };
  const cache = new WorkbenchCache<Draft>();
  cache.set("A", { text: "sent", original: "before" });
  cache.set("B", { text: "other", original: "other" });
  const unsubscribe = cache.subscribe("A", () => {});
  let finish!: () => void;
  const saved = new Promise<void>((resolve) => (finish = resolve)).then(() => {
    cache.set("A", { ...cache.get("A")!, original: "sent" });
  });
  unsubscribe();
  cache.set("A", { text: "newer typing", original: "before" });
  let current = cache.get("A");
  const close = cache.subscribe("A", (state) => (current = state));
  let otherNotified = false;
  cache.subscribe("B", () => (otherNotified = true));
  finish();
  await saved;
  assert.deepEqual(current, { text: "newer typing", original: "sent" });
  assert.equal(otherNotified, false);
  assert.equal(cache.get("B")!.text, "other");
  close();
  assert.equal(cache.hasSubscribers("A"), false);
});
