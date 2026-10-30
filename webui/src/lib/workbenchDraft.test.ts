import test from "node:test";
import assert from "node:assert/strict";
import { draftFromFile, draftContent, draftDirty } from "./workbenchDraft.ts";
test("editing preserves CRLF without marking the newly opened file dirty", () => {
  const draft = draftFromFile("first\r\nsecond\r\n", "revision");
  assert.equal(draftDirty(draft), false);
  draft.text += "third\n";
  assert.equal(draftDirty(draft), true);
  assert.equal(draftContent(draft), "first\r\nsecond\r\nthird\r\n");
  assert.equal(draft.revision, "revision");
});
test("LF and a UTF-8 BOM survive a read/save round trip", () => {
  const content = "\ufeffhello\n";
  assert.equal(draftContent(draftFromFile(content, "revision")), content);
});
