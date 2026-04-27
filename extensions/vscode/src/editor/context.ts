import * as vscode from 'vscode';
import * as path from 'path';

export interface EditorContext {
  filePath?: string;
  fileName?: string;
  language?: string;
  selection?: string;
  startLine?: number;
  endLine?: number;
}

export function getEditorContext(): EditorContext {
  const editor = vscode.window.activeTextEditor;
  if (!editor) return {};

  const selection = editor.selection;
  const hasSelection = !selection.isEmpty;

  return {
    filePath: editor.document.uri.fsPath,
    fileName: path.basename(editor.document.uri.fsPath),
    language: editor.document.languageId,
    selection: hasSelection ? editor.document.getText(selection) : undefined,
    startLine: hasSelection ? selection.start.line + 1 : undefined,
    endLine: hasSelection ? selection.end.line + 1 : undefined,
  };
}

export function buildContextualPrompt(action: string, context: EditorContext): string {
  if (!context.selection || !context.fileName) {
    return action;
  }

  const location = context.startLine === context.endLine
    ? `line ${context.startLine}`
    : `lines ${context.startLine}-${context.endLine}`;

  return `File: ${context.fileName} (${context.language || 'unknown'})\nSelected code (${location}):\n\`\`\`${context.language || ''}\n${context.selection}\n\`\`\`\n\n${action}`;
}
