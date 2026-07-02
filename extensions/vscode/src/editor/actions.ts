import * as vscode from 'vscode';

export class AtomCodeActionProvider implements vscode.CodeActionProvider {
  static readonly providedCodeActionKinds = [vscode.CodeActionKind.QuickFix, vscode.CodeActionKind.Refactor];

  provideCodeActions(
    _document: vscode.TextDocument,
    range: vscode.Range | vscode.Selection,
  ): vscode.CodeAction[] {
    if (range.isEmpty) return [];

    const actions: vscode.CodeAction[] = [];

    const explainAction = new vscode.CodeAction(vscode.l10n.t('AtomCode: Explain'), vscode.CodeActionKind.Empty);
    explainAction.command = { command: 'atomcode.explain', title: vscode.l10n.t('Explain Selection') };
    actions.push(explainAction);

    const fixAction = new vscode.CodeAction(vscode.l10n.t('AtomCode: Fix'), vscode.CodeActionKind.QuickFix);
    fixAction.command = { command: 'atomcode.fix', title: vscode.l10n.t('Fix Selection') };
    actions.push(fixAction);

    const optimizeAction = new vscode.CodeAction(vscode.l10n.t('AtomCode: Optimize'), vscode.CodeActionKind.Refactor);
    optimizeAction.command = { command: 'atomcode.optimize', title: vscode.l10n.t('Optimize Selection') };
    actions.push(optimizeAction);

    const addToChatAction = new vscode.CodeAction(vscode.l10n.t('AtomCode: Add to Chat'), vscode.CodeActionKind.Empty);
    addToChatAction.command = { command: 'atomcode.addToChat', title: vscode.l10n.t('Add to Chat') };
    actions.push(addToChatAction);

    return actions;
  }
}
