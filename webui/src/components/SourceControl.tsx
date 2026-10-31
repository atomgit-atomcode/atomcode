// Git source-control panel (P4 webui side).
//
// The repository on disk is the authority: this component holds no git state
// beyond the last response, and every action re-reads `/git/status`. It fits
// the existing `.modal-overlay` / `.modal-card` family, so the narrow-screen
// rule turns it into the same bottom drawer as every other panel.

import { useEffect, useRef, useState } from 'preact/hooks';
import {
  gitStatus,
  gitInit,
  gitStage,
  gitUnstage,
  gitCommit,
  gitPull,
  gitPush,
  gitClone,
  gitDiff,
  changeDir,
  listGitCredentials,
  storeGitCredential,
  removeGitCredential,
  GitStatus,
  GitCredential,
  GitDiffKind,
  GitDiffResult,
  CdResponse,
} from '../api';
import { useT } from '../settings';

/** Turn anything thrown by the API client into one readable line. */
function errText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

interface SourceControlProps {
  onClose: () => void;
  onPick: (path: string, sessionId?: string) => void;
}

export function SourceControl({ onClose, onPick }: SourceControlProps) {
  const t = useT();
  const [status, setStatus] = useState<GitStatus | null>(null);
  const [loading, setLoading] = useState(true);
  // Label of the command currently in flight ('' = none). Disables the buttons
  // so two operations cannot race on the same index.
  const [busy, setBusy] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [output, setOutput] = useState('');
  const [message, setMessage] = useState('');
  const [cloneUrl, setCloneUrl] = useState('');
  const [cloneDir, setCloneDir] = useState('');
  // Credentials: hosts + accounts only; the token is write-only.
  const [credentials, setCredentials] = useState<GitCredential[]>([]);
  const [credentialHost, setCredentialHost] = useState('');
  const [credentialUser, setCredentialUser] = useState('');
  const [credentialToken, setCredentialToken] = useState('');
  const [diffKey, setDiffKey] = useState<string | null>(null);
  const [diff, setDiff] = useState<GitDiffResult | null>(null);
  const [diffLoading, setDiffLoading] = useState(false);
  const [diffError, setDiffError] = useState<string | null>(null);
  const diffRequest = useRef(0);

  function clearDiff() {
    setDiffKey(null);
    setDiff(null);
    setDiffError(null);
    setDiffLoading(false);
  }

  async function reload() {
    diffRequest.current += 1;
    clearDiff();
    try {
      setStatus(await gitStatus());
    } catch (failure) {
      setError(errText(failure));
    }
  }

  async function reloadCredentials() {
    try {
      setCredentials(await listGitCredentials());
    } catch (failure) {
      setError(errText(failure));
    }
  }

  useEffect(() => {
    let live = true;
    gitStatus()
      .then((next) => {
        if (live) setStatus(next);
      })
      .catch((failure) => {
        if (live) setError(errText(failure));
      })
      .finally(() => {
        if (live) setLoading(false);
      });
    listGitCredentials()
      .then((next) => {
        if (live) setCredentials(next);
      })
      .catch((failure) => {
        if (live) setError(errText(failure));
      });
    return () => {
      live = false;
      diffRequest.current += 1;
    };
  }, []);

  /** Run one git action, then re-read the repository and the credential list. */
  async function run(label: string, action: () => Promise<{ output?: string }>) {
    if (busy) return false;
    setBusy(label);
    setError(null);
    setNotice(null);
    setOutput('');
    try {
      const result = await action();
      setOutput(result?.output ?? '');
      await reload();
      await reloadCredentials();
      return true;
    } catch (failure) {
      setError(errText(failure));
      // A failed command may still have changed the repository (a conflict, a
      // half-completed pull), so never leave the panel showing stale status.
      await reload();
      return false;
    } finally {
      setBusy('');
    }
  }

  async function toggleDiff(path: string, kind: GitDiffKind) {
    const key = `${kind}:${path}`;
    if (diffKey === key) {
      diffRequest.current += 1;
      clearDiff();
      return;
    }
    const request = ++diffRequest.current;
    setDiffKey(key);
    setDiff(null);
    setDiffError(null);
    setDiffLoading(true);
    try {
      const result = await gitDiff(path, kind);
      if (diffRequest.current === request) setDiff(result);
    } catch (failure) {
      if (diffRequest.current === request) setDiffError(errText(failure));
    } finally {
      if (diffRequest.current === request) setDiffLoading(false);
    }
  }

  function renderDiffResult(result: GitDiffResult) {
    return (
      <div class="scm-diff-result">
        {result.binary && <p class="scm-note">{t('scm.binaryDiff')}</p>}
        {!result.binary && (
          <pre class="scm-diff-patch">{result.patch || t('scm.emptyDiff')}</pre>
        )}
        {result.truncated && <p class="scm-hint">{t('scm.truncatedDiff')}</p>}
      </div>
    );
  }

  const changes = status?.changes ?? [];
  const conflicts = changes.filter((change) => change.conflicted);
  const staged = changes.filter((change) => change.staged);
  const unstaged = changes.filter((change) => change.unstaged);
  const hasRepo = status?.repo === true;
  const hasRemotes = (status?.remotes.length ?? 0) > 0;
  const disabled = busy !== '';

  async function handleCommit() {
    const text = message.trim();
    if (!text) {
      setError(t('scm.commitEmpty'));
      return;
    }
    const ok = await run(t('scm.commit'), () => gitCommit(text));
    if (ok) {
      setMessage('');
      setNotice(t('scm.commitDone'));
    }
  }

  async function handleInit() {
    const ok = await run(t('scm.init'), () => gitInit());
    if (ok) setNotice(t('scm.initDone'));
  }

  async function handleClone() {
    const url = cloneUrl.trim();
    if (!url) {
      setError(t('scm.cloneUrlRequired'));
      return;
    }
    if (busy) return;
    setBusy(t('scm.clone'));
    setError(null);
    setNotice(null);
    setOutput('');
    try {
      const cloned = await gitClone(url, cloneDir.trim() || undefined);
      setOutput(cloned.output);
      let opened: CdResponse;
      try {
        opened = await changeDir(cloned.root);
      } catch (failure) {
        setError(t('scm.cloneCdFailed', { root: cloned.root, error: errText(failure) }));
        return;
      }
      if (!opened.success) {
        setError(t('scm.cloneCdFailed', { root: cloned.root, error: opened.message }));
        return;
      }
      setCloneUrl('');
      setCloneDir('');
      onPick(opened.current_dir || cloned.root, opened.session_id);
      onClose();
    } catch (failure) {
      setError(errText(failure));
    } finally {
      setBusy('');
    }
  }

  async function handleAddCredential() {
    const host = credentialHost.trim();
    const username = credentialUser.trim();
    const token = credentialToken.trim();
    if (!host || !username || !token) {
      setError(t('scm.credentialRequired'));
      return;
    }
    setBusy(t('scm.saveCredential'));
    setError(null);
    setNotice(null);
    try {
      const saved = await storeGitCredential(host, username, token);
      // Never keep the token in component state after it is stored.
      setCredentialToken('');
      setCredentialHost('');
      setCredentialUser('');
      await reloadCredentials();
      setNotice(t('scm.credentialSaved', { host: saved.host }));
    } catch (failure) {
      setError(errText(failure));
    } finally {
      setBusy('');
    }
  }

  async function handleRemoveCredential(host: string) {
    setBusy(t('scm.removeCredential'));
    setError(null);
    setNotice(null);
    try {
      await removeGitCredential(host);
      await reloadCredentials();
    } catch (failure) {
      setError(errText(failure));
    } finally {
      setBusy('');
    }
  }

  function renderChange(path: string, code: string, kind: GitDiffKind, orig?: string) {
    const key = `${kind}:${path}`;
    const expanded = diffKey === key;
    return (
      <div class="scm-change" key={key}>
        <div class="scm-row">
          <span class="scm-row-code" title={code}>
            {code.trim() || '·'}
          </span>
          <button
            type="button"
            class="scm-row-path scm-diff-trigger"
            title={path}
            aria-expanded={expanded}
            onClick={() => void toggleDiff(path, kind)}
          >
            {path}
            {orig && <span class="scm-row-orig"> ← {orig}</span>}
          </button>
          {kind !== 'conflict' && (
            <button
              type="button"
              class="btn scm-row-action"
              disabled={disabled}
              onClick={() =>
                void run(
                  kind === 'staged' ? t('scm.unstage') : t('scm.stage'),
                  () => (kind === 'staged' ? gitUnstage([path]) : gitStage([path])),
                )
              }
            >
              {kind === 'staged' ? t('scm.unstage') : t('scm.stage')}
            </button>
          )}
        </div>
        {expanded && (
          <div class="scm-diff">
            {diffLoading && <p class="scm-note" role="status">{t('scm.diffLoading')}</p>}
            {diffError && <p class="scm-note scm-error" role="alert">{diffError}</p>}
            {diff && renderDiffResult(diff)}
          </div>
        )}
      </div>
    );
  }

  return (
    <div
      class="modal-overlay"
      onClick={(event) => {
        if (event.target === event.currentTarget) onClose();
      }}
    >
      <div class="modal-card source-control">
        <div class="modal-header">
          <span aria-hidden="true">⑂</span>
          <h3>{t('scm.title')}</h3>
          {hasRepo && status?.root && <span class="modal-sub">{status.root}</span>}
          <button class="btn modal-close" onClick={onClose} aria-label={t('scm.close')}>
            ✕
          </button>
        </div>

        <div class="modal-body">
          {loading && <p class="scm-note" role="status">{t('scm.loading')}</p>}

          {!loading && !hasRepo && (
            <section class="scm-section">
              <p class="scm-note">{status?.error || t('scm.notRepo')}</p>
              <p class="scm-hint">{t('scm.notRepoHint')}</p>
              <div class="scm-actions">
                <button class="btn" disabled={disabled} onClick={() => void handleInit()}>
                  {busy === t('scm.init') ? t('scm.working') : t('scm.init')}
                </button>
              </div>
              <div class="scm-field-group">
                <span class="modal-label">{t('scm.clone')}</span>
                <input
                  class="menu-input"
                  type="text"
                  value={cloneUrl}
                  placeholder="https://atomgit.com/owner/repo.git"
                  onInput={(event) => setCloneUrl((event.target as HTMLInputElement).value)}
                  onKeyDown={(event) => {
                    if (event.isComposing) return;
                    if (event.key === 'Enter') void handleClone();
                  }}
                />
                <input
                  class="menu-input"
                  type="text"
                  value={cloneDir}
                  placeholder={t('scm.cloneDir')}
                  onInput={(event) => setCloneDir((event.target as HTMLInputElement).value)}
                />
                <div class="scm-actions">
                  <button class="btn btn-primary" disabled={disabled} onClick={() => void handleClone()}>
                    {busy === t('scm.clone') ? t('scm.working') : t('scm.clone')}
                  </button>
                </div>
              </div>
            </section>
          )}

          {hasRepo && status && (
            <>
              <section class="scm-repo-row">
                <span class="scm-branch" title={status.branch || t('scm.detached')}>
                  {status.branch || t('scm.detached')}
                </span>
                <span class="scm-upstream">
                  {status.upstream || t('scm.noUpstream')}
                </span>
                {status.upstream && (
                  <span class="scm-ahead-behind">
                    ↑{status.ahead} ↓{status.behind}
                  </span>
                )}
                <button
                  class="btn scm-refresh"
                  disabled={disabled}
                  onClick={() => void run(t('scm.refresh'), () => Promise.resolve({ output: '' }))}
                >
                  {t('scm.refresh')}
                </button>
              </section>

              {conflicts.length > 0 && (
                <section class="scm-section scm-conflicts">
                  <span class="modal-label">
                    {t('scm.conflicts')} · {conflicts.length}
                  </span>
                  {conflicts.map((change) =>
                    renderChange(change.path, change.status, 'conflict', change.orig_path),
                  )}
                </section>
              )}

              <section class="scm-section">
                <span class="modal-label">
                  {t('scm.staged')} · {staged.length}
                </span>
                {staged.length === 0 && <p class="scm-note">{t('scm.noStaged')}</p>}
                {staged.map((change) =>
                  renderChange(change.path, change.status, 'staged', change.orig_path),
                )}
              </section>

              <section class="scm-section">
                <span class="modal-label">
                  {t('scm.changes')} · {unstaged.length}
                </span>
                {unstaged.length === 0 && <p class="scm-note">{t('scm.noChanges')}</p>}
                {unstaged.map((change) =>
                  renderChange(
                    change.path,
                    change.status,
                    change.untracked ? 'untracked' : 'worktree',
                    change.orig_path,
                  ),
                )}
                {unstaged.length > 1 && (
                  <div class="scm-actions">
                    <button
                      class="btn"
                      disabled={disabled}
                      onClick={() =>
                        void run(t('scm.stageAll'), () =>
                          gitStage(unstaged.map((change) => change.path)),
                        )
                      }
                    >
                      {t('scm.stageAll')}
                    </button>
                  </div>
                )}
              </section>

              <section class="scm-section">
                <span class="modal-label">{t('scm.commit')}</span>
                <textarea
                  class="menu-input scm-message"
                  rows={3}
                  value={message}
                  placeholder={t('scm.commitMessage')}
                  onInput={(event) => setMessage((event.target as HTMLTextAreaElement).value)}
                />
                <div class="scm-actions">
                  <button
                    class="btn btn-primary"
                    disabled={disabled || !message.trim() || staged.length === 0 || conflicts.length > 0}
                    onClick={() => void handleCommit()}
                  >
                    {busy === t('scm.commit') ? t('scm.working') : t('scm.commit')}
                  </button>
                </div>
                {conflicts.length > 0 && <p class="scm-hint">{t('scm.commitConflictHint')}</p>}
                {conflicts.length === 0 && staged.length === 0 && (
                  <p class="scm-hint">{t('scm.commitHint')}</p>
                )}
              </section>

              <section class="scm-section">
                <div class="scm-actions">
                  <button
                    class="btn"
                    disabled={disabled || !hasRemotes || !status.branch}
                    onClick={() => void run(t('scm.pull'), () => gitPull())}
                  >
                    {busy === t('scm.pull') ? t('scm.working') : t('scm.pull')}
                  </button>
                  <button
                    class="btn"
                    disabled={disabled || !hasRemotes || !status.branch}
                    onClick={() => void run(t('scm.push'), () => gitPush())}
                  >
                    {busy === t('scm.push') ? t('scm.working') : t('scm.push')}
                  </button>
                </div>
              </section>

              <section class="scm-section">
                <span class="modal-label">{t('scm.remotes')}</span>
                {status.remotes.length === 0 && <p class="scm-note">{t('scm.noRemotes')}</p>}
                {status.remotes.map((remote) => (
                  <div class="scm-row" key={remote.name}>
                    <span class="scm-remote-name">{remote.name}</span>
                    <span class="scm-row-path mono" title={remote.url}>
                      {remote.url}
                    </span>
                  </div>
                ))}
              </section>
            </>
          )}

          <section class="scm-section">
            <span class="modal-label">{t('scm.credentials')}</span>
            <p class="scm-hint">{t('scm.credentialsHint')}</p>
            {credentials.length === 0 && <p class="scm-note">{t('scm.noCredentials')}</p>}
            {credentials.map((credential) => (
              <div class="scm-row" key={credential.host}>
                <span class="scm-remote-name mono">{credential.host}</span>
                <span class="scm-row-path">{credential.username}</span>
                <button
                  type="button"
                  class="btn scm-row-action"
                  disabled={disabled}
                  onClick={() => void handleRemoveCredential(credential.host)}
                >
                  {t('scm.removeCredential')}
                </button>
              </div>
            ))}
            <div class="scm-field-group">
              <input
                class="menu-input"
                type="text"
                value={credentialHost}
                placeholder={t('scm.host')}
                onInput={(event) => setCredentialHost((event.target as HTMLInputElement).value)}
              />
              <input
                class="menu-input"
                type="text"
                value={credentialUser}
                placeholder={t('scm.username')}
                onInput={(event) => setCredentialUser((event.target as HTMLInputElement).value)}
              />
              <input
                class="menu-input"
                type="password"
                value={credentialToken}
                placeholder={t('scm.token')}
                autocomplete="off"
                onInput={(event) => setCredentialToken((event.target as HTMLInputElement).value)}
                onKeyDown={(event) => {
                  if (event.isComposing) return;
                  if (event.key === 'Enter') void handleAddCredential();
                }}
              />
              <div class="scm-actions">
                <button
                  class="btn btn-primary"
                  disabled={disabled || !credentialHost.trim() || !credentialUser.trim() || !credentialToken.trim()}
                  onClick={() => void handleAddCredential()}
                >
                  {t('scm.saveCredential')}
                </button>
              </div>
            </div>
          </section>

          {notice && <p class="scm-note scm-ok" role="status">{notice}</p>}
          {error && <p class="scm-note scm-error" role="alert">{error}</p>}
          {output && <pre class="scm-output">{output}</pre>}
        </div>

        <div class="modal-footer">
          <button class="btn" onClick={onClose}>{t('scm.close')}</button>
        </div>
      </div>
    </div>
  );
}