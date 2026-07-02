import * as vscode from 'vscode';

export class StatusBarManager {
  private item: vscode.StatusBarItem;
  private _model = '';

  constructor() {
    this.item = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Right, 100);
    this.item.command = 'atomcode.openPreferredLocation';
    this.item.tooltip = vscode.l10n.t('AtomCode: Click to open chat');
    this.update(false);
    this.item.show();
  }

  update(connected: boolean, model?: string, tokens?: number) {
    if (model) this._model = model;
    void tokens;

    if (connected) {
      this.item.text = '$(hubot) AtomCode';
      this.item.tooltip = this._model
        ? vscode.l10n.t('AtomCode: Connected ({model})', { model: this._model })
        : vscode.l10n.t('AtomCode: Connected');
    } else {
      this.item.text = '$(hubot) AtomCode ○';
      this.item.tooltip = vscode.l10n.t('AtomCode: Not connected — click to retry');
    }
  }

  dispose() { this.item.dispose(); }
}
