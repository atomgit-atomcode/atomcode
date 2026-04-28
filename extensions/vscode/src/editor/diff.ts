import * as vscode from 'vscode';
import * as path from 'path';

const originalContents = new Map<string, string>();

export class DiffContentProvider implements vscode.TextDocumentContentProvider {
  private _onDidChange = new vscode.EventEmitter<vscode.Uri>();
  readonly onDidChange = this._onDidChange.event;

  provideTextDocumentContent(uri: vscode.Uri): string {
    const filePath = uri.query;
    return originalContents.get(filePath) || '';
  }

  static storeOriginal(filePath: string, content: string) {
    originalContents.set(filePath, content);
  }

  static clearOriginal(filePath: string) {
    originalContents.delete(filePath);
  }
}

export async function showDiff(filePath: string, originalContent: string) {
  DiffContentProvider.storeOriginal(filePath, originalContent);

  const originalUri = vscode.Uri.parse(`atomcode-original:${path.basename(filePath)}?${filePath}`);
  const modifiedUri = vscode.Uri.file(filePath);

  await vscode.commands.executeCommand('vscode.diff',
    originalUri,
    modifiedUri,
    `AtomCode: ${path.basename(filePath)} (changes)`,
    { preview: true }
  );
}
