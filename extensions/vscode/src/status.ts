import * as vscode from 'vscode';

export class StatusBarManager {
  private item: vscode.StatusBarItem;
  private _connected = false;
  private _model = '';
  private _tokens = 0;

  constructor() {
    this.item = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Right, 100);
    this.item.command = 'atomcode.openPreferredLocation';
    this.item.tooltip = 'AtomCode: Click to open chat';
    this.update(false);
    this.item.show();
  }

  update(connected: boolean, model?: string, tokens?: number) {
    this._connected = connected;
    if (model) this._model = model;
    if (tokens !== undefined) this._tokens = tokens;

    if (connected) {
      const modelDisplay = this._model || 'AtomCode';
      const tokenDisplay = this._tokens > 0 ? ` · ${this.formatTokens(this._tokens)}` : '';
      this.item.text = `$(hubot) ${modelDisplay}${tokenDisplay}`;
      this.item.tooltip = `AtomCode: Connected (${modelDisplay})`;
    } else {
      this.item.text = '$(hubot) AtomCode ○';
      this.item.tooltip = 'AtomCode: Not connected — click to retry';
    }
  }

  updateTokens(tokens: number) {
    this._tokens = tokens;
    if (this._connected) this.update(true);
  }

  private formatTokens(n: number): string {
    if (n >= 1000000) return `${(n / 1000000).toFixed(1)}M`;
    if (n >= 1000) return `${(n / 1000).toFixed(1)}k`;
    return String(n);
  }

  dispose() { this.item.dispose(); }
}
