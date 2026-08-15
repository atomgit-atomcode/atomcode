// Two-column layout: sidebar + chat, header with cwd breadcrumb + config.
// VSCode design system: timeline messages, violet brand, floating input.

import { useEffect, useRef, useState } from 'preact/hooks';
import { Chat } from './components/Chat';
import { Sidebar } from './components/Sidebar';
import { ThemeDialog, LanguageDialog, ModelConfigDialog, RemoteAccessDialog } from './components/SettingsDialogs';
import { RenameDialog, DeleteDialog } from './components/SessionDialogs';
import { CwdPicker } from './components/CwdPicker';
import { resolvePendingAfterDecision } from './lib/pendingPermission';
import { getProject, getConfig, changeDir, resolveSession, createSession, getSession, SessionMetaWithProject } from './api';
import { useT, SettingsSection } from './settings';
import { sessionMessagesToMarkdownLines } from './lib/historyMessages';

// 从 URL (?session=<id>) 读取要打开的会话 id（短 id），用于刷新后恢复。
function readSessionIdFromUrl(): string | null {
  try {
    return new URLSearchParams(window.location.search).get('session');
  } catch {
    return null;
  }
}

// URL 里只放 UUID 前 8 位以缩短地址；刷新时按前缀在会话列表里还原成完整 id。
function shortSessionId(id: string): string {
  return id.slice(0, 8);
}

export function App() {
  const t = useT();
  // URL 里是短 id，不能直接当完整 id 用（后端需要完整 id）；先置空，挂载后再还原。
  const [sessionId, setSessionId] = useState<string | null>(null);
  const [activeSession, setActiveSession] = useState<SessionMetaWithProject | null>(null);
  // 首条消息发出瞬间插入的「乐观会话」：让新会话即时出现在侧栏（后端空会话不入列表，
  // 真实标题也要等回合落盘后自动命名）。其 id 与真实会话对齐后，列表刷新会用真实条目
  // 覆盖它（Sidebar 按 id 去重，真实条目优先），标题随之从「前 10 字」换成自动命名。
  const [optimisticSession, setOptimisticSession] = useState<SessionMetaWithProject | null>(null);
  const [cwd, setCwd] = useState('');
  // Physical session-bucket hash of the current project. The sidebar scopes its
  // list by this (see Sidebar `projectHash`), not by the `cwd` string, so a
  // session whose stored `working_dir` was restamped by the daemon's global
  // working_dir can't leak into the wrong project. Tracked alongside `cwd`.
  const [projectHash, setProjectHash] = useState('');
  const [pending, setPending] = useState<any | null>(null);
  const [showCwd, setShowCwd] = useState(false);
  const [settingsSection, setSettingsSection] = useState<SettingsSection | null>(null);
  const [sidebarOpen, setSidebarOpen] = useState(false);
  const [sidebarCollapsed, setSidebarCollapsed] = useState(false);
  const [sessionListVersion, setSessionListVersion] = useState(0);
  const [headerMenuOpen, setHeaderMenuOpen] = useState(false);
  // 表头会话菜单改用 fixed 定位，避免被祖先 overflow/层叠裁剪；记录锚点坐标。
  const [headerMenuPos, setHeaderMenuPos] = useState<{ top: number; left: number } | null>(null);
  const [headerDialog, setHeaderDialog] = useState<'rename' | 'delete' | null>(null);
  const [headerExporting, setHeaderExporting] = useState(false);
  const headerMenuRef = useRef<HTMLDivElement>(null);
  // Chat reports whether it is showing the centered landing (empty) state, so the
  // session-title header can hide on landing (matching the design's full-bleed hero).
  const [isLanding, setIsLanding] = useState(true);
  // Skills picked from the sidebar Skills menu → bumped so Chat inserts `/name `.
  const [skillInsert, setSkillInsert] = useState<{ name: string; seq: number } | null>(null);
  // 挂载时 URL 里的（短）session id；用 ref 暂存，避免被 URL 同步 effect 清掉。
  const urlSessionRef = useRef<string | null>(readSessionIdFromUrl());
  // 跳过首次 URL 同步：此时可能正等待把短 id 还原成完整会话，别先清掉参数。
  const firstUrlSync = useRef(true);
  // 刷新时若 URL 带 session id，先进入「恢复中」态：在按短 id 还原出会话前，
  // 抑制 Chat 的新建落地页，避免「先闪一下新建页再跳到历史」的体验。
  const [restoring, setRestoring] = useState<boolean>(() => readSessionIdFromUrl() != null);

  // 关闭表头会话菜单：外部点击 / 滚动 / 缩放（fixed 菜单不跟随锚点，故一并关闭）。
  useEffect(() => {
    if (!headerMenuOpen) return;
    const close = () => setHeaderMenuOpen(false);
    const onDown = (e: MouseEvent) => {
      if (headerMenuRef.current && !headerMenuRef.current.contains(e.target as Node)) {
        setHeaderMenuOpen(false);
      }
    };
    document.addEventListener('mousedown', onDown);
    window.addEventListener('resize', close);
    window.addEventListener('scroll', close, true);
    return () => {
      document.removeEventListener('mousedown', onDown);
      window.removeEventListener('resize', close);
      window.removeEventListener('scroll', close, true);
    };
  }, [headerMenuOpen]);

  // Chat 完成首条消息后会回传它创建的 session id；若是新 id，刷新侧栏列表。
  function handleSessionAssigned(id: string) {
    // 把乐观条目的 id 对齐到后端真实 id（落地页发送时乐观条目用的是临时 id），
    // 这样接下来的列表刷新能按 id 去重，用真实条目（含自动命名）覆盖乐观条目。
    setOptimisticSession((prev) => (prev && prev.id !== id ? { ...prev, id } : prev));
    if (id !== sessionId) {
      setSessionId(id);
      setSessionListVersion((v) => v + 1);
    }
  }

  // 首条消息发出瞬间：用消息前 10 字做临时标题，乐观插入侧栏（即时可见）。
  // 优先复用已创建会话的 id / 目录（新建按钮、切目录会预建会话）；落地页直发时
  // 还没有 id，先用临时 id，待 done 回传真实 id 后由 handleSessionAssigned 对齐。
  function handleOptimisticSession(title: string) {
    const now = Math.floor(Date.now() / 1000);
    setOptimisticSession({
      id: activeSession?.id ?? `optimistic-${now}`,
      name: title,
      working_dir: activeSession?.working_dir ?? cwd,
      // Fall back to the current project's hash (not '') so the optimistic row
      // survives the sidebar's project_hash filter on a landing-page first send.
      project_hash: activeSession?.project_hash ?? projectHash,
      created_at: activeSession?.created_at ?? now,
      updated_at: now,
      message_count: 1,
    });
    // 顶部标题头用的是 activeSession.name；首发时同步成临时标题（已有会话元数据时），
    // 否则标题头会一直停在占位名 session-<ts>。落地页首发无 activeSession，标题头本就
    // 隐藏，待回合结束列表刷新后由 handleActiveSessionMeta 回填真实标题。
    // 但如果用户已手动重命名过会话（名称非 session-xxx 格式），则不覆盖，保护用户选择。
    setActiveSession((prev) =>
      prev
        ? {
            ...prev,
            name: prev.name.startsWith('session-') ? title : prev.name,
          }
        : prev,
    );
  }

  // 侧栏列表（重新）加载后回传当前会话的真实元数据：回合落盘后后端已自动命名，
  // 用真实标题覆盖顶部标题头的临时标题（前 10 字）。仅在仍是同一会话且标题有变时更新。
  function handleActiveSessionMeta(s: SessionMetaWithProject) {
    setActiveSession((prev) =>
      prev && prev.id === s.id && prev.name !== s.name ? { ...prev, name: s.name } : prev,
    );
  }

  // ☰：移动端专用，开/关抽屉。桌面端的收起/展开由侧栏自身的按钮处理。
  function toggleSidebar() {
    setSidebarOpen((o) => !o);
  }

  // Seed cwd from /project on mount（恢复会话时以会话目录为准，故只在仍为空时填充）
  useEffect(() => {
    let cancelled = false;
    getProject()
      .then((p) => {
        if (cancelled) return;
        if (p.working_dir) setCwd((c) => c || p.working_dir);
        if (p.project_hash) setProjectHash((h) => h || p.project_hash!);
      })
      .catch(() => {
        // Ignore; cwd stays empty
      });
    return () => { cancelled = true; };
  }, []);

  // 把当前 session id（取前 8 位）同步进 URL，刷新后可恢复。
  useEffect(() => {
    // 跳过首次：挂载时若 URL 已带短 id 而 sessionId 尚未还原，别先把参数清掉。
    if (firstUrlSync.current) {
      firstUrlSync.current = false;
      return;
    }
    const url = new URL(window.location.href);
    if (sessionId) {
      url.searchParams.set('session', shortSessionId(sessionId));
    } else {
      url.searchParams.delete('session');
    }
    window.history.replaceState(null, '', url.pathname + url.search + url.hash);
  }, [sessionId]);

  // 刷新后用 URL 里的短 id 还原成完整记录（完整 id + project_hash + working_dir），
  // 再交给 Chat 加载历史。用 resolveSession（跨所有桶按 id 定位，不受 /sessions 的
  // 50 条上限影响），否则较旧的会话刷新后会找不到、回落到新建页。仅在挂载时执行一次。
  useEffect(() => {
    const urlSid = urlSessionRef.current;
    if (!urlSid) return;
    let cancelled = false;
    resolveSession(urlSid)
      .then((found) => {
        if (cancelled || !found) return;
        setSessionId(found.id);
        setActiveSession(found);
        if (found.working_dir) setCwd(found.working_dir);
        if (found.project_hash) setProjectHash(found.project_hash);
      })
      .catch(() => {
        /* 找不到就维持现状（回到新建页） */
      })
      .finally(() => {
        // 无论找没找到，恢复流程结束：解除抑制（找到→历史页，没找到→新建页）。
        if (!cancelled) setRestoring(false);
      });
    return () => {
      cancelled = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // 当 sessionId 改变，且 activeSession 的 id 与之不匹配时，自动解析/加载该会话的元数据。
  // 解决了在 TUI 或其他端中执行 /resume 切换会话、或新建会话后，webui 无法同步更新
  // activeSession、CWD 以及 projectHash 的问题。
  useEffect(() => {
    if (!sessionId) return;
    if (activeSession && activeSession.id === sessionId) return;

    let cancelled = false;
    resolveSession(sessionId)
      .then((found) => {
        if (cancelled || !found) return;
        setActiveSession(found);
        if (found.working_dir) setCwd(found.working_dir);
        if (found.project_hash) setProjectHash(found.project_hash);
        setSessionListVersion((v) => v + 1);
      })
      .catch((err) => {
        console.warn('[syncSession] resolve session failed:', err);
      });

    return () => {
      cancelled = true;
    };
  }, [sessionId]);

  // 在指定目录下创建一个新会话并切过去（落地、侧栏可见）。
  // 先重置画布回落地页给即时反馈，再异步建会话；失败则停在落地页。
  function openNewSession(targetCwd: string | undefined) {
    setSessionId(null);
    setActiveSession(null);
    setOptimisticSession(null);
    // 仅在同步开启（?sync=1）时让后端广播新建，使 sync 模式 TUI 跟随；
    // 关闭同步时 webui 新建对话不应牵连 TUI 新建（issue #850）。
    let sync = false;
    try { sync = new URLSearchParams(location.search).get('sync') === '1'; } catch { /* ignore */ }
    createSession(targetCwd || undefined, undefined, sync)
      .then((data) => {
        if (data.project_hash) setProjectHash(data.project_hash);
        setSessionId(data.id);
        setActiveSession({
          id: data.id,
          name: data.name,
          working_dir: data.working_dir,
          project_hash: data.project_hash,
          created_at: data.created_at,
          updated_at: data.created_at,
          message_count: 0,
        });
        setSessionListVersion((v) => v + 1);
      })
      .catch((err) => {
        // 创建失败时已在前端落地页，仅打印日志
        console.warn('[newSession] create session failed:', err);
      });
  }

  function handleNewSession() {
    setSidebarOpen(false);
    openNewSession(cwd || undefined);
  }

  function handleSelectSession(session: SessionMetaWithProject) {
    setOptimisticSession(null);
    setSessionId(session.id);
    setActiveSession(session);
    if (session.working_dir) {
      setCwd(session.working_dir);
    }
    // Scope the sidebar to the picked session's physical project bucket.
    if (session.project_hash) setProjectHash(session.project_hash);
    setSidebarOpen(false);
  }

  // 切换工作目录：侧栏按新目录过滤会话，并在该目录下新建一个会话（落地、侧栏可见）。
  // 也是「切换项目」下拉选中另一个项目时的入口（真正切进去，而非只浏览）。
  function handlePickCwd(path: string) {
    setSidebarOpen(false);
    setCwd(path);
    // We don't know the new dir's bucket hash until the daemon creates a session
    // there; clear it so the sidebar falls back to filtering by `cwd` in the
    // meantime, then openNewSession's response re-pins projectHash.
    setProjectHash('');
    openNewSession(path || undefined);
  }

  // 当从外部（TUI `/cd` 等）收到工作目录变更通知时：
  // 更新 cwd 并拉取最新的项目信息以更新 projectHash。
  function handleCwdChanged(newCwd: string) {
    setCwd(newCwd);
    getProject()
      .then((p) => {
        if (p.project_hash) {
          setProjectHash(p.project_hash);
        }
        setSessionListVersion((v) => v + 1);
      })
      .catch((err) => {
        console.warn('[cwdChanged] failed to get project state:', err);
      });
  }

  // 删除会话：若删的是当前打开的会话，回到空白新对话。
  function handleSessionDeleted(id: string) {
    if (id === sessionId) {
      setSessionId(null);
      setActiveSession(null);
    }
  }

  // 重命名会话：若是当前会话，同步更新标题。
  function handleSessionRenamed(id: string, name: string) {
    if (id === sessionId) {
      setActiveSession((prev) => (prev ? { ...prev, name } : prev));
    }
  }

  /** Export the active session's conversation as a Markdown file. */
  async function handleExportMarkdown() {
    if (!activeSession) return;
    setHeaderMenuOpen(false);
    setHeaderExporting(true);
    try {
      const detail = await getSession(activeSession.project_hash, activeSession.id);
      const title = detail.name || detail.id.slice(0, 8);
      const lines = sessionMessagesToMarkdownLines(detail.messages, title);
      const mdContent = lines.join('\n');
      const blob = new Blob([mdContent], { type: 'text/markdown;charset=utf-8' });
      const url = URL.createObjectURL(blob);
      const a = document.createElement('a');
      a.href = url;
      a.download = `${title.replace(/[\\/:*?"<>|]/g, '_')}.md`;
      document.body.appendChild(a);
      a.click();
      document.body.removeChild(a);
      URL.revokeObjectURL(url);
    } catch {
      alert(t('sidebar.exportFailed'));
    } finally {
      setHeaderExporting(false);
    }
  }

  return (
    <div class="app">
      {/* ===== Full-height sidebar (通栏)：品牌 + 收起按钮在其顶部 ===== */}
      <Sidebar
        activeSessionId={sessionId}
        onSelect={handleSelectSession}
        onNew={handleNewSession}
        open={sidebarOpen}
        collapsed={sidebarCollapsed}
        onToggleCollapse={() => setSidebarCollapsed((c) => !c)}
        onOpenSettings={(section) => setSettingsSection(section)}
        reloadKey={sessionListVersion}
        optimisticSession={optimisticSession}
        onActiveSessionMeta={handleActiveSessionMeta}
        cwd={cwd}
        projectHash={projectHash}
        onSessionRenamed={handleSessionRenamed}
        onSessionDeleted={handleSessionDeleted}
        onPickSkill={(name) => setSkillInsert({ name, seq: Date.now() })}
        onOpenRemote={() => setSettingsSection('remote')}
        onOpenCwd={() => setShowCwd(true)}
        onSwitchProject={handlePickCwd}
      />
      <div
        class={'sidebar-backdrop' + (sidebarOpen ? ' show' : '')}
        onClick={() => setSidebarOpen(false)}
      />

      {/* ===== Main column: sticky session-title header + chat (no top bar) ===== */}
      <div class="main-column">
        {/* Mobile-only floating menu button (the old top bar carried the ☰; the
            redesign has no top bar, so a fixed button gives mobile drawer access). */}
        <button
          class="mobile-menu-btn"
          onClick={toggleSidebar}
          aria-label={t('header.menu')}
          title={t('header.sessionList')}
        >
          ☰
        </button>

        {activeSession?.name && !isLanding && (
          <header class="session-header" ref={headerMenuRef}>
            <button
              class="session-title-btn"
              title={activeSession.name}
              onClick={(e) => {
                if (headerMenuOpen) {
                  setHeaderMenuOpen(false);
                  return;
                }
                const r = (e.currentTarget as HTMLElement).getBoundingClientRect();
                setHeaderMenuPos({ top: r.bottom + 4, left: r.left });
                setHeaderMenuOpen(true);
              }}
            >
              <span class="session-title-text">{activeSession.name}</span>
              <svg
                class="session-title-chevron"
                width="11"
                height="11"
                viewBox="0 0 16 16"
                fill="none"
                aria-hidden="true"
              >
                <path
                  d="M4 6l4 4 4-4"
                  stroke="currentColor"
                  stroke-width="1.5"
                  stroke-linecap="round"
                  stroke-linejoin="round"
                />
              </svg>
            </button>
            {headerMenuOpen && headerMenuPos && (
              <div
                class="item-menu"
                style={{ top: `${headerMenuPos.top}px`, left: `${headerMenuPos.left}px` }}
              >
                <button
                  class="item-menu-row"
                  onClick={() => {
                    setHeaderMenuOpen(false);
                    setHeaderDialog('rename');
                  }}
                >
                  <span>{t('sidebar.rename')}</span>
                </button>
                <button
                  class="item-menu-row"
                  onClick={handleExportMarkdown}
                  disabled={headerExporting}
                >
                  <span>{headerExporting ? t('sidebar.exporting') : t('sidebar.exportMarkdown')}</span>
                </button>
                <button
                  class="item-menu-row danger"
                  onClick={() => {
                    setHeaderMenuOpen(false);
                    setHeaderDialog('delete');
                  }}
                >
                  <span>{t('sidebar.delete')}</span>
                </button>
              </div>
            )}
          </header>
        )}

        <div class="session-body app-sidebar">
          <Chat
            sessionId={sessionId}
            onSessionId={handleSessionAssigned}
            cwd={cwd}
            onPermission={setPending}
            pendingPermission={pending}
            onPermissionResolved={(callId) =>
              setPending((cur: any) =>
                callId === null ? null : resolvePendingAfterDecision(cur, callId),
              )}
            activeSession={activeSession}
            restoring={restoring}
            onLiveTurnDone={() => setSessionListVersion((v) => v + 1)}
            onOptimisticSession={handleOptimisticSession}
            onOpenCwd={() => setShowCwd(true)}
            onCwdChanged={handleCwdChanged}
            onLanding={setIsLanding}
            skillInsert={skillInsert}
            onSessionRenamed={(name) => setActiveSession((prev) => prev ? { ...prev, name } : prev)}
          />
        </div>
      </div>

      {/* ===== Modals ===== */}
      {showCwd && (
        <CwdPicker
          current={cwd}
          onPick={handlePickCwd}
          onClose={() => setShowCwd(false)}
        />
      )}
      {settingsSection === 'theme' && (
        <ThemeDialog onClose={() => setSettingsSection(null)} />
      )}
      {settingsSection === 'language' && (
        <LanguageDialog onClose={() => setSettingsSection(null)} />
      )}
      {settingsSection === 'model' && (
        <ModelConfigDialog onClose={() => setSettingsSection(null)} />
      )}
      {settingsSection === 'remote' && (
        <RemoteAccessDialog onClose={() => setSettingsSection(null)} />
      )}
      {headerDialog === 'rename' && activeSession && (
        <RenameDialog
          session={activeSession}
          onClose={() => setHeaderDialog(null)}
          onDone={(name) => {
            handleSessionRenamed(activeSession.id, name);
            setSessionListVersion((v) => v + 1);
          }}
        />
      )}
      {headerDialog === 'delete' && activeSession && (
        <DeleteDialog
          session={activeSession}
          onClose={() => setHeaderDialog(null)}
          onDone={() => {
            handleSessionDeleted(activeSession.id);
            setSessionListVersion((v) => v + 1);
          }}
        />
      )}
    </div>
  );
}
