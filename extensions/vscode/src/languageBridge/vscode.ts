import * as vscode from 'vscode';
import * as path from 'node:path';
import { Document, EditorHost, Position, QueryError, QueryOperation } from './query';
import { startBridge } from './server';

const LIMIT = 200;
function range(r: vscode.Range) {
  return { line: r.start.line + 1, character: r.start.character + 1, end_line: r.end.line + 1, end_character: r.end.character + 1 };
}
function locations(items: Array<vscode.Location | vscode.LocationLink>) {
  return items.slice(0, LIMIT + 1).map(item => 'targetUri' in item
    ? { uri: item.targetUri.toString(), ...range(item.targetSelectionRange ?? item.targetRange) }
    : { uri: item.uri.toString(), ...range(item.range) });
}
function symbols(items: Array<vscode.DocumentSymbol | vscode.SymbolInformation>) {
  let remaining = LIMIT;
  let truncated = false;
  const visit = (list: Array<vscode.DocumentSymbol | vscode.SymbolInformation>, depth: number): object[] => {
    if (depth > 20) { truncated ||= list.length > 0; return []; }
    const output: object[] = [];
    for (const item of list) {
      if (remaining-- <= 0) { truncated = true; break; }
      output.push('location' in item
        ? { name: item.name.slice(0, 1000), kind: item.kind, uri: item.location.uri.toString(), ...range(item.location.range) }
        : { name: item.name.slice(0, 1000), kind: item.kind, detail: item.detail.slice(0, 1000), ...range(item.selectionRange), children: visit(item.children, depth + 1) });
    }
    return output;
  };
  const outline = visit(items, 0);
  if (truncated) outline.push({ truncated: true });
  return outline;
}
class VscodeHost implements EditorHost {
  async open(filePath: string): Promise<Document> {
    const doc = await vscode.workspace.openTextDocument(vscode.Uri.file(filePath));
    if (doc.getText().length > 4 * 1024 * 1024) throw new QueryError('document_too_large', 'Editor document exceeds the bridge limit.');
    return { uri: doc.uri.toString(), languageId: doc.languageId, version: () => doc.version, dirty: () => doc.isDirty, lines: () => doc.getText().split(/\r?\n/u) };
  }
  async query(operation: QueryOperation, doc: Document, position: Position | undefined, severity: string): Promise<unknown[]> {
    const uri = vscode.Uri.parse(doc.uri);
    const at = position ? new vscode.Position(position.line, position.character) : undefined;
    switch (operation) {
      case 'hover': {
        const items = await vscode.commands.executeCommand<vscode.Hover[]>('vscode.executeHoverProvider', uri, at) ?? [];
        let remaining = 12000;
        return items.slice(0, LIMIT + 1).map(item => {
          let truncated = item.contents.length > LIMIT;
          const contents = item.contents.slice(0, LIMIT).map(content => {
            const value = typeof content === 'string' ? content : content.value;
            const text = value.slice(0, Math.max(0, remaining));
            truncated ||= text.length < value.length;
            remaining -= text.length;
            return text;
          });
          return { contents, truncated };
        });
      }
      case 'definition':
        return locations(await vscode.commands.executeCommand<Array<vscode.Location | vscode.LocationLink>>('vscode.executeDefinitionProvider', uri, at) ?? []);
      case 'references':
        return locations(await vscode.commands.executeCommand<vscode.Location[]>('vscode.executeReferenceProvider', uri, at) ?? []);
      case 'document_symbols':
        return symbols(await vscode.commands.executeCommand<Array<vscode.DocumentSymbol | vscode.SymbolInformation>>('vscode.executeDocumentSymbolProvider', uri) ?? []);
      case 'diagnostics': {
        const maxSeverity = severity === 'all' ? vscode.DiagnosticSeverity.Hint : severity === 'warning' ? vscode.DiagnosticSeverity.Warning : vscode.DiagnosticSeverity.Error;
        return vscode.languages.getDiagnostics(uri).filter(item => item.severity <= maxSeverity)
          .sort((a, b) => a.severity - b.severity || a.range.start.line - b.range.start.line)
          .slice(0, LIMIT + 1).map(item => ({ ...range(item.range), severity: ['error', 'warning', 'information', 'hint'][item.severity], message: item.message.slice(0, 4000), truncated: item.message.length > 4000, source: item.source?.slice(0, 1000), code: typeof item.code === 'object' ? item.code.value : item.code }));
      }
    }
  }
}

/** Explicit user command; queries do not depend on daemon availability. */
export function registerLanguageBridge(context: vscode.ExtensionContext) {
  let bridge: Awaited<ReturnType<typeof startBridge>> | undefined;
  let starting: Promise<Awaited<ReturnType<typeof startBridge>>> | undefined;
  let epoch = 0;
  const stop = async () => {
    epoch++;
    const pending = starting;
    starting = undefined;
    const current = bridge;
    bridge = undefined;
    await current?.dispose();
    if (pending) await pending.then(value => value.dispose(), () => {});
  };
  context.subscriptions.push(
    vscode.commands.registerCommand('atomcode.enableLanguageBridge', async () => {
      let pending: typeof starting;
      try {
        if (!vscode.workspace.isTrusted) throw new Error('Trust this workspace before enabling the language bridge.');
        const roots = vscode.workspace.workspaceFolders?.map(folder => folder.uri).filter(uri => uri.scheme === 'file') ?? [];
        if (!roots.length || context.globalStorageUri.scheme !== 'file') throw new Error('A filesystem workspace is required on this extension host.');
        if (bridge && !bridge.active) bridge = undefined;
        if (!bridge) {
          const generation = epoch;
          if (!starting) starting = startBridge(path.join(context.globalStorageUri.fsPath, 'language-bridge'), roots.map(uri => uri.fsPath), new VscodeHost(), vscode.env.appName);
          pending = starting;
          const value = await pending;
          if (generation !== epoch) { await value.dispose(); return; }
          bridge = value;
          if (starting === pending) starting = undefined;
        }
        await vscode.env.clipboard.writeText(`[lsp]\nide_connection_file = ${JSON.stringify(bridge.connectionFile)}\n`);
        void vscode.window.showInformationMessage(vscode.l10n.t('IDE language bridge enabled. Connection configuration copied; merge it into your AtomCode config and reopen the session.'));
      } catch {
        if (starting === pending) starting = undefined;
        void vscode.window.showErrorMessage(vscode.l10n.t('Could not enable the IDE language bridge. Open a trusted filesystem workspace and try again.'));
      }
    }),
    vscode.commands.registerCommand('atomcode.disableLanguageBridge', stop),
    vscode.workspace.onDidChangeWorkspaceFolders(() => { void stop(); }),
    { dispose: () => { void stop(); } },
  );
}
