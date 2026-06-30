import React, { useCallback, useEffect, useMemo, useRef } from 'react';
import DOMPurify from 'dompurify';
import { ArtifactData } from '../state/types';
import { renderCodeBlockHtml } from './codeBlockRendering';
import { DiffView } from './DiffView';
import { classifyArtifactRenderKind, normalizeCodeArtifactContent } from './artifactRendering';

function looksLikeUnifiedPatch(content: string): boolean {
  return /^diff --git /m.test(content) || /^@@ /m.test(content) || /^--- /m.test(content) || /^\+\+\+ /m.test(content);
}

export function ArtifactCodeView({ artifact }: { artifact: ArtifactData }) {
  const containerRef = useRef<HTMLDivElement>(null);
  const showDiffView = classifyArtifactRenderKind(artifact) === 'diff' && looksLikeUnifiedPatch(artifact.content);

  const handleActions = useCallback((event: MouseEvent) => {
    const target = event.target as HTMLElement;
    const btn = target.closest('.copy-button') as HTMLElement | null;
    if (!btn) return;
    const wrapper = btn.closest('.code-block-wrapper') as HTMLElement | null;
    if (!wrapper) return;
    const codeEl = wrapper.querySelector('pre code');
    if (!codeEl) return;
    const code = wrapper.dataset.rawCode ?? codeEl.textContent ?? '';
    navigator.clipboard.writeText(code).then(() => {
      btn.title = 'Copied!';
      setTimeout(() => { btn.title = 'Copy'; }, 2000);
    });
  }, []);

  useEffect(() => {
    const el = containerRef.current;
    if (!el) return;
    el.addEventListener('click', handleActions);
    return () => el.removeEventListener('click', handleActions);
  }, [handleActions]);

  const html = useMemo(() => {
    const normalized = normalizeCodeArtifactContent(artifact.content, artifact.language);
    return DOMPurify.sanitize(renderCodeBlockHtml(normalized.content, normalized.language));
  }, [artifact.content, artifact.language]);

  if (showDiffView) {
    return (
      <div className="artifact-diff">
        <DiffView content={artifact.content} />
      </div>
    );
  }

  return (
    <div ref={containerRef} className="artifact-code-render" dangerouslySetInnerHTML={{ __html: html }} />
  );
}
