/** Draft data is UI state, never injected into the model or native session log. */
export interface WorkbenchDraft {
  text: string;
  original: string;
  revision: string;
  crlf: boolean;
  cursor?: number;
  scroll?: number;
}
export function draftFromFile(
  content: string,
  revision: string,
): WorkbenchDraft {
  const text = content.replace(/\r\n/g, "\n");
  const crlf =
    content.includes("\r\n") && !content.replace(/\r\n/g, "").includes("\n");
  return { text, original: text, revision, crlf };
}
export function draftContent(draft: WorkbenchDraft): string {
  return draft.crlf ? draft.text.replace(/\n/g, "\r\n") : draft.text;
}
export function draftDirty(draft?: WorkbenchDraft): boolean {
  return !!draft && draft.text !== draft.original;
}
