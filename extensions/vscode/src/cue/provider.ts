import * as vscode from 'vscode';
import { DaemonClient } from '../daemon/client';
import {
  buildCompletionContext,
  isCompletableDocument,
  trimContext,
} from './context';

/**
 * CUE — AI inline completion for VS Code.
 *
 * The extension shows the model's answer as GHOST TEXT at the cursor, and the
 * user accepts it with Tab (the TRAE / Copilot inline-completion shape).
 *
 * Three rules shape this file:
 *
 * 1. **The daemon owns the credential.** The extension never holds an API
 *    key — it asks the daemon, which already knows the model and the account.
 *    So this file carries code, never secrets.
 *
 * 2. **A late suggestion is worse than no suggestion.** The user keeps
 *    typing while the model answers, so an answer that arrives for an
 *    outdated cursor position must be DROPPED, not shown. Every request is
 *    therefore tagged, and an answer is only shown when its tag is still the
 *    current one for that document.
 *
 * 3. **The suggestion is INSERTED, never pasted over.** The daemon returns
 *    the text to insert at the cursor.
 *
 * The debounce is VS Code's own (`editor.inlineSuggest.delay`): the editor
 * asks for a completion after the user stops typing, and cancels the request
 * when they resume. A second timer here would only add latency.
 *
 * The rules about WHAT the model sees live in `context.ts`, which does not
 * import `vscode` — so they can be tested without an editor.
 */

/**
 * A request in flight.
 *
 * The identity of this object IS the ticket: an answer is only shown when its
 * request is still the current one for that document. That covers every way
 * the cursor can move on — a new keystroke, a cursor move, a new request for
 * the same version — without comparing positions, which the editor reports
 * in a shape that would have to be re-derived on every change.
 */
interface PendingRequest {
  controller: AbortController;
  /** Disposed when the request finishes, so the listener stops firing. */
  subscription: vscode.Disposable;
}

export class CueCompletionProvider implements vscode.InlineCompletionItemProvider {
  /** The request in flight for each document. */
  private pending = new WeakMap<vscode.TextDocument, PendingRequest>();
  /** Every request in flight, so turning CUE off can cancel all of them. */
  private active = new Set<PendingRequest>();
  private enabled = true;

  constructor(private readonly client: DaemonClient) {}

  /** Turn CUE on or off in this window. */
  setEnabled(enabled: boolean): void {
    this.enabled = enabled;
    if (!enabled) this.cancelAll();
  }

  isEnabled(): boolean {
    return this.enabled;
  }

  /** Drop every request in flight — their answers no longer match a cursor. */
  private cancelAll(): void {
    for (const request of this.active) {
      request.controller.abort();
    }
  }

  async provideInlineCompletionItems(
    document: vscode.TextDocument,
    position: vscode.Position,
    _context: vscode.InlineCompletionContext,
    token: vscode.CancellationToken,
  ): Promise<vscode.InlineCompletionItem[] | vscode.InlineCompletionList | undefined> {
    if (!this.enabled || token.isCancellationRequested) return undefined;
    const text = document.getText();
    if (!isCompletableDocument(document.languageId, document.uri.scheme, text)) {
      return undefined;
    }

    // A completion request is a POST: the daemon sees the client go away when
    // the editor cancels it.
    const controller = new AbortController();
    const request: PendingRequest = {
      controller,
      subscription: token.onCancellationRequested(() => controller.abort()),
    };
    // The newest request wins: the one before it can no longer be shown.
    this.pending.get(document)?.controller.abort();
    this.pending.set(document, request);
    this.active.add(request);

    try {
      const offset = document.offsetAt(position);
      const context = trimContext(buildCompletionContext(text, offset));
      if (!context.prefix && !context.suffix) return undefined;

      const response = await this.client.requestCueCompletion(
        {
          file_path: document.uri.fsPath,
          language_id: document.languageId,
          prefix: context.prefix,
          suffix: context.suffix,
        },
        controller.signal,
      );

      // The answer belongs to a cursor position that no longer exists.
      if (controller.signal.aborted) return undefined;
      if (token.isCancellationRequested) return undefined;
      if (this.pending.get(document) !== request) return undefined;

      const completion = response.completion;
      if (!completion) return undefined;

      // An EMPTY range at the cursor: the suggestion is inserted, never
      // pasted over what the user typed.
      const item = new vscode.InlineCompletionItem(completion, new vscode.Range(position, position));
      return [item];
    } catch {
      // A failed completion is not an error the user needs to see: the editor
      // simply shows no suggestion.
      return undefined;
    } finally {
      request.subscription.dispose();
      this.active.delete(request);
      if (this.pending.get(document) === request) {
        this.pending.delete(document);
      }
    }
  }
}

/**
 * Register CUE with the editor.
 *
 * The provider is registered for every language — the decision of WHAT to
 * complete is made inside the provider, where the document is known.
 */
export function registerCueCompletion(
  context: vscode.ExtensionContext,
  client: DaemonClient,
): CueCompletionProvider {
  const provider = new CueCompletionProvider(client);
  context.subscriptions.push(
    vscode.languages.registerInlineCompletionItemProvider({ pattern: '**' }, provider),
  );
  return provider;
}
