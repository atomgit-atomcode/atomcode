import type { MsgPart } from './toolRows';

export interface TurnArtifact {
  path: string;
  label: string;
}

interface ArtifactMessage {
  role: 'user' | 'assistant' | 'system';
  parts: MsgPart[];
}

const SINGLE_FILE_MUTATORS = new Set(['write_file', 'edit_file']);

function basename(path: string): string {
  const normalized = path.replace(/\\/g, '/').replace(/\/+$/, '');
  return normalized.slice(normalized.lastIndexOf('/') + 1) || path;
}

function searchReplacePaths(output: string): string[] {
  const paths: string[] = [];
  for (const line of output.split(/\r?\n/)) {
    // `search_replace` reports one authoritative line per file after all writes:
    // "  /path/to/file (3 replacements)". Do not infer paths from its requested
    // root/glob because a successful no-match call must produce no artifact.
    const match = /^\s{2}(.+) \(\d+ replacements\)$/.exec(line);
    if (match?.[1]?.trim()) paths.push(match[1].trim());
  }
  return paths;
}

function successfulMutationPaths(parts: MsgPart[]): string[] {
  const paths: string[] = [];
  for (const part of parts) {
    // A history snapshot can contain the assistant call without its matching
    // tool-result record. `status` used to default to done during hydration, so
    // require the authoritative result payload as well as the success status.
    if (
      part.kind !== 'tool' ||
      part.tool.status !== 'done' ||
      part.tool.output === undefined
    ) continue;
    let args: unknown;
    try {
      args = JSON.parse(part.tool.args);
    } catch {
      continue;
    }
    if (!args || typeof args !== 'object') continue;
    const record = args as Record<string, unknown>;
    if (SINGLE_FILE_MUTATORS.has(part.tool.name)) {
      if (typeof record.file_path === 'string' && record.file_path.trim()) {
        paths.push(record.file_path.trim());
      }
      continue;
    }
    if (part.tool.name === 'search_replace') {
      paths.push(...searchReplacePaths(part.tool.output));
      continue;
    }
    if (part.tool.name === 'parallel_edit_files' && Array.isArray(record.files)) {
      for (const file of record.files) {
        if (!file || typeof file !== 'object') continue;
        const path = (file as Record<string, unknown>).path;
        if (typeof path === 'string' && path.trim()) paths.push(path.trim());
      }
    }
  }
  return paths;
}

/**
 * Associate every assistant message with the successful file mutations from its
 * complete user turn. System notices are transparent, matching Chat's turn grouping.
 */
export function artifactsByAssistantIndex(messages: ArtifactMessage[]): Map<number, TurnArtifact[]> {
  const result = new Map<number, TurnArtifact[]>();
  let assistantIndexes: number[] = [];
  let paths: string[] = [];

  const flush = () => {
    if (assistantIndexes.length === 0) return;
    const seen = new Set<string>();
    const artifacts = paths
      .filter((path) => {
        if (seen.has(path)) return false;
        seen.add(path);
        return true;
      })
      .map((path) => ({ path, label: basename(path) }));
    for (const index of assistantIndexes) result.set(index, artifacts);
    assistantIndexes = [];
    paths = [];
  };

  messages.forEach((message, index) => {
    if (message.role === 'user') {
      flush();
    } else if (message.role === 'assistant') {
      assistantIndexes.push(index);
      paths.push(...successfulMutationPaths(message.parts));
    }
  });
  flush();
  return result;
}
