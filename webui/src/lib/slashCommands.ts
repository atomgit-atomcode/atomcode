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
