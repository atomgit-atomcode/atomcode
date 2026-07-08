export interface ParsedCommand {
  name: string;
  arg: string;
}

// 命令名字符集与 TUI parse_slash_line 对齐：首字符字母，后续字母/数字/_-: 。
// 因为遇到空白即停，"/Users/me" 的 name 会含 '/' → 不匹配 → 判为非命令（路径回退聊天）。
const NAME_RE = /^[A-Za-z][A-Za-z0-9_:-]*$/;

/** 解析一行输入为斜杠命令；非命令返回 null（调用方应作为普通聊天发送）。 */
export function parseSlashCommand(line: string): ParsedCommand | null {
  const trimmed = line.trim();
  if (!trimmed.startsWith('/')) return null;
  const body = trimmed.slice(1);
  const spaceIdx = body.search(/\s/);
  const name = spaceIdx === -1 ? body : body.slice(0, spaceIdx);
  if (!NAME_RE.test(name)) return null;
  const arg = spaceIdx === -1 ? '' : body.slice(spaceIdx + 1).trim();
  return { name, arg };
}

export type ApprovalMode = 'build' | 'plan' | 'bypass';

/** Chat.tsx 注入的既有页面能力；命令 run() 只调用这些，不含自身业务逻辑。 */
export interface SlashHandlers {
  setMode(mode: ApprovalMode): void | Promise<void>;
  openModelPicker(): void;
  setProvider(name: string): void | Promise<void>;
  changeDir(path: string): void | Promise<void>;
  openSessionSidebar(): void;
  reloadConfig(): void | Promise<void>;
  openSlashSkillsMenu(): void;
  notice(text: string): void;
  t(key: string, params?: Record<string, string | number>): string;
}

export interface SlashCommandDef {
  name: string;
  aliases?: string[];
  /** i18n key，用于菜单描述与 /help。 */
  descKey: string;
  argHint?: string;
  run(arg: string, h: SlashHandlers): void | Promise<void>;
}

export const FRONTEND_COMMANDS: SlashCommandDef[] = [
  { name: 'plan', descKey: 'cmd.plan.desc', run: (_a, h) => h.setMode('plan') },
  { name: 'build', descKey: 'cmd.build.desc', run: (_a, h) => h.setMode('build') },
  {
    name: 'model',
    descKey: 'cmd.model.desc',
    argHint: '[name]',
    run: (a, h) => (a ? h.setProvider(a) : h.openModelPicker()),
  },
  {
    name: 'cd',
    descKey: 'cmd.cd.desc',
    argHint: '<path>',
    run: (a, h) => {
      if (!a) { h.notice(h.t('cmd.cd.needArg')); return; }
      return h.changeDir(a);
    },
  },
  { name: 'resume', descKey: 'cmd.resume.desc', run: (_a, h) => h.openSessionSidebar() },
  { name: 'reload', descKey: 'cmd.reload.desc', run: (_a, h) => h.reloadConfig() },
  { name: 'skills', descKey: 'cmd.skills.desc', run: (_a, h) => h.openSlashSkillsMenu() },
  { name: 'help', descKey: 'cmd.help.desc', run: (_a, h) => h.notice(buildHelpText(h.t)) },
];

export function buildCommandMap(defs: SlashCommandDef[]): Map<string, SlashCommandDef> {
  const m = new Map<string, SlashCommandDef>();
  for (const d of defs) {
    m.set(d.name, d);
    for (const a of d.aliases ?? []) m.set(a, d);
  }
  return m;
}

export function buildHelpText(t: (key: string, params?: Record<string, string | number>) => string): string {
  const lines = [t('cmd.help.title')];
  for (const d of FRONTEND_COMMANDS) {
    lines.push(`/${d.name}${d.argHint ? ' ' + d.argHint : ''} — ${t(d.descKey)}`);
  }
  return lines.join('\n');
}

export interface DispatchResult {
  handled: boolean;
  unknown?: boolean;
}

/** 解析并分发一行。非命令 → {handled:false}；未知命令 → {handled:false,unknown:true}（两者调用方都应作为普通聊天发送）。 */
export async function dispatchSlashCommand(
  line: string,
  map: Map<string, SlashCommandDef>,
  h: SlashHandlers,
): Promise<DispatchResult> {
  const parsed = parseSlashCommand(line);
  if (!parsed) return { handled: false };
  const def = map.get(parsed.name);
  if (!def) return { handled: false, unknown: true };
  await def.run(parsed.arg, h);
  return { handled: true };
}

export interface SlashMenuItem {
  name: string;
  description: string;
  kind: 'command' | 'skill';
}

export function buildSlashMenuItems(
  commands: SlashCommandDef[],
  skills: { name: string; description?: string }[],
  query: string,
  t: (key: string) => string,
): SlashMenuItem[] {
  const q = query.toLowerCase();
  const cmdItems: SlashMenuItem[] = commands
    .filter((c) => c.name.toLowerCase().includes(q))
    .map((c) => ({ name: c.name, description: t(c.descKey), kind: 'command' as const }));
  const skillItems: SlashMenuItem[] = skills
    .filter((s) => s.name.toLowerCase().includes(q))
    .map((s) => ({ name: s.name, description: s.description ?? '', kind: 'skill' as const }))
    .sort((a, b) => a.name.localeCompare(b.name));
  return [...cmdItems, ...skillItems];
}
