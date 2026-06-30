import { ArtifactData } from '../state/types';

export type ArtifactRenderKind = 'markdown' | 'code' | 'diff';

const CODE_LANGUAGE_ALIASES = new Set([
  'bash',
  'c',
  'cpp',
  'css',
  'go',
  'html',
  'java',
  'javascript',
  'js',
  'json',
  'jsx',
  'python',
  'py',
  'rust',
  'rs',
  'sh',
  'shell',
  'sql',
  'svg',
  'tsx',
  'ts',
  'typescript',
  'vue',
  'xml',
  'yaml',
  'yml',
]);

const GENERIC_ARTIFACT_TITLES = new Set([
  'artifact',
  'code',
  'diff',
  'markdown',
  'patch',
  'plain',
  'plaintext',
  'text',
  'txt',
]);

const WELL_KNOWN_EXTENSIONLESS_FILENAMES = new Set([
  'dockerfile',
  'makefile',
  'readme',
]);

function normalize(value: string | undefined): string {
  return (value ?? '').trim().toLowerCase().replace(/^\./, '');
}

function titleExtension(title: string | undefined): string {
  return normalize(title?.match(/\.([a-z0-9]+)$/i)?.[1]);
}

function hasMeaningfulTitle(title: string | undefined): boolean {
  const normalizedTitle = normalize(title);
  if (!normalizedTitle || GENERIC_ARTIFACT_TITLES.has(normalizedTitle) || CODE_LANGUAGE_ALIASES.has(normalizedTitle)) {
    return false;
  }
  return Boolean(titleExtension(title) || title?.includes('/') || title?.includes('\\') || WELL_KNOWN_EXTENSIONLESS_FILENAMES.has(normalizedTitle));
}

function isTextLike(value: string): boolean {
  return value === '' || value === 'text' || value === 'plain' || value === 'plaintext' || value === 'txt';
}

function isMarkdownLike(value: string): boolean {
  return value === 'markdown' || value === 'md';
}

function isDiffLike(value: string): boolean {
  return value === 'diff' || value === 'patch';
}

function isDiffSentinelLine(line: string): boolean {
  return /^(diff|patch)$/i.test(line.trim());
}

function isDiffMetaLine(line: string): boolean {
  return (
    line.startsWith('diff --git ') ||
    line.startsWith('index ') ||
    line.startsWith('new file mode ') ||
    line.startsWith('deleted file mode ') ||
    line.startsWith('old mode ') ||
    line.startsWith('new mode ') ||
    line.startsWith('similarity index ') ||
    line.startsWith('dissimilarity index ') ||
    line.startsWith('rename from ') ||
    line.startsWith('rename to ') ||
    line.startsWith('copy from ') ||
    line.startsWith('copy to ') ||
    line.startsWith('--- ') ||
    line.startsWith('+++ ')
  );
}

function contentLooksLikeDiff(content: string): boolean {
  const lines = content
    .replace(/\n$/, '')
    .split('\n')
    .filter((line) => !isDiffSentinelLine(line));
  const nonEmptyLines = lines.filter((line) => line.trim().length > 0);
  if (nonEmptyLines.length === 0) return false;

  const hasChangedLine = nonEmptyLines.some((line) => line.startsWith('+') || line.startsWith('-'));
  if (!hasChangedLine) return false;

  return nonEmptyLines.every((line) =>
    line.startsWith('+') ||
    line.startsWith('-') ||
    line.startsWith(' ') ||
    line.startsWith('@@') ||
    isDiffMetaLine(line),
  );
}

function isCodeLanguage(language: string, extension: string): boolean {
  return CODE_LANGUAGE_ALIASES.has(language) || CODE_LANGUAGE_ALIASES.has(extension);
}

function isLanguageSentinelLine(line: string): boolean {
  return CODE_LANGUAGE_ALIASES.has(normalize(line));
}

function nextNonEmptyLine(lines: string[], startIndex: number): string | undefined {
  for (let i = startIndex; i < lines.length; i += 1) {
    if (lines[i].trim().length > 0) return lines[i];
  }
  return undefined;
}

function nextLineLooksLikeCode(lines: string[], startIndex: number): boolean {
  const line = nextNonEmptyLine(lines, startIndex);
  if (!line) return false;
  return looksLikeCodeLine(line);
}

function looksLikeCodeLine(line: string): boolean {
  const trimmed = line.trim();
  if (!trimmed) return false;
  if (/^\s/.test(line)) return true;
  if (/^(import|export|const|let|var|function|class|interface|type|enum|return|if|for|while|switch|case|try|catch|async|await|public|private|protected|static|this\.|\/\/|\/\*|\*|#|@)\b/.test(trimmed)) {
    return true;
  }
  if (/^[A-Za-z_$][\w$.-]*\s*[:=]/.test(trimmed)) return true;
  return /[{}()[\];=<>|]/.test(trimmed);
}

function isFenceLine(line: string): boolean {
  return /^\s*(```|~~~)/.test(line);
}

export function normalizeMarkdownArtifactContent(content: string): string {
  const lines = content.replace(/\r\n?/g, '\n').split('\n');
  const output: string[] = [];
  let inFence = false;

  for (let i = 0; i < lines.length;) {
    const line = lines[i];
    if (isFenceLine(line)) {
      inFence = !inFence;
      output.push(line);
      i += 1;
      continue;
    }

    if (!inFence && isLanguageSentinelLine(line) && nextLineLooksLikeCode(lines, i + 1)) {
      output.push(`\`\`\`${normalize(line)}`);
      i += 1;

      let closed = false;
      while (i < lines.length) {
        const current = lines[i];

        if (isLanguageSentinelLine(current) && nextLineLooksLikeCode(lines, i + 1)) {
          output.push('```');
          closed = true;
          break;
        }

        if (current.trim().length === 0) {
          const next = nextNonEmptyLine(lines, i + 1);
          if (next && !isLanguageSentinelLine(next) && !looksLikeCodeLine(next)) {
            output.push('```');
            output.push(current);
            i += 1;
            closed = true;
            break;
          }
        }

        output.push(current);
        i += 1;
      }

      if (!closed) {
        output.push('```');
      }
      continue;
    }

    output.push(line);
    i += 1;
  }

  return output.join('\n');
}

export function normalizeCodeArtifactContent(content: string, language?: string): { content: string; language?: string } {
  const lines = content.replace(/\r\n?/g, '\n').split('\n');
  const output: string[] = [];
  let detectedLanguage = normalize(language);

  for (let i = 0; i < lines.length; i += 1) {
    const line = lines[i];
    const sentinelLanguage = normalize(line);
    if (
      isLanguageSentinelLine(line) &&
      nextLineLooksLikeCode(lines, i + 1) &&
      (isTextLike(detectedLanguage) || !detectedLanguage || sentinelLanguage === detectedLanguage)
    ) {
      if (isTextLike(detectedLanguage) || !detectedLanguage) {
        detectedLanguage = sentinelLanguage;
      }
      continue;
    }
    output.push(line);
  }

  return {
    content: output.join('\n').replace(/^\n+/, ''),
    language: detectedLanguage || language,
  };
}

export function shouldRenderArtifactChrome(artifact: ArtifactData): boolean {
  return hasMeaningfulTitle(artifact.title);
}

export function classifyArtifactRenderKind(artifact: ArtifactData): ArtifactRenderKind {
  const artifactType = normalize(artifact.artifactType);
  const language = normalize(artifact.language);
  const extension = titleExtension(artifact.title);

  if (isDiffLike(artifactType) || isDiffLike(language) || isDiffLike(extension) || contentLooksLikeDiff(artifact.content)) {
    return 'diff';
  }

  if (isMarkdownLike(artifactType) || isMarkdownLike(language) || isMarkdownLike(extension)) {
    return 'markdown';
  }

  if (isCodeLanguage(language, extension)) {
    return 'code';
  }

  if (isTextLike(artifactType) && isTextLike(language)) {
    return 'markdown';
  }

  return 'code';
}
