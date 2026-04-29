import * as vscode from 'vscode';

export class AtomCodeActionProvider implements vscode.CodeActionProvider {
  static readonly providedCodeActionKinds = [vscode.CodeActionKind.QuickFix, vscode.CodeActionKind.Refactor];

  provideCodeActions(
    _document: vscode.TextDocument,
    range: vscode.Range | vscode.Selection,
  ): vscode.CodeAction[] {
    if (range.isEmpty) return [];

    const actions: vscode.CodeAction[] = [];

    const explainAction = new vscode.CodeAction('AtomCode: Explain', vscode.CodeActionKind.Empty);
    explainAction.command = { command: 'atomcode.explain', title: 'Explain Selection' };
    actions.push(explainAction);

    const fixAction = new vscode.CodeAction('AtomCode: Fix', vscode.CodeActionKind.QuickFix);
    fixAction.command = { command: 'atomcode.fix', title: 'Fix Selection' };
    actions.push(fixAction);

    const optimizeAction = new vscode.CodeAction('AtomCode: Optimize', vscode.CodeActionKind.Refactor);
    optimizeAction.command = { command: 'atomcode.optimize', title: 'Optimize Selection' };
    actions.push(optimizeAction);

    return actions;
  }
}
