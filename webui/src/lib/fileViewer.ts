import { createContext } from 'preact';

/** `content` opens the file; `diff` shows its current Git changes. */
export type FileViewerView = 'content' | 'diff';

/** A local file the chat (or a workspace document) asks to show. The session
 *  is the one whose message or document holds the link, never a guess. */
export interface FileViewerRequest {
  sessionId: string;
  path: string;
  view: FileViewerView;
}

export interface FileViewerRoute {
  open: (request: FileViewerRequest) => void;
}

/** Owned by App, which routes each request to the workspace of the session it
 *  names and drops requests for any session that is no longer displayed.
 *  Neutral on purpose: Markdown reads it without importing the Workbench. */
export const FileViewerContext = createContext<FileViewerRoute | null>(null);
