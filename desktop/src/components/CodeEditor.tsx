import { useCallback, useEffect, useRef, useState } from 'react';
import Editor, { OnMount } from '@monaco-editor/react';

interface CodeEditorProps {
  filePath: string;
  content: string;
  language: string;
  apiBaseUrl: string;
  onSave: (path: string, content: string) => Promise<void>;
}

function extToMonacoLang(ext: string): string {
  const map: Record<string, string> = {
    js: 'javascript', jsx: 'javascript', ts: 'typescript', tsx: 'typescript',
    py: 'python', rs: 'rust', json: 'json', html: 'html', htm: 'html',
    css: 'css', scss: 'scss', less: 'less', md: 'markdown', mdx: 'markdown',
    xml: 'xml', svg: 'xml', yaml: 'yaml', yml: 'yaml', toml: 'plaintext',
    go: 'go', java: 'java', c: 'c', cpp: 'cpp', h: 'c', hpp: 'cpp',
    sh: 'shell', bash: 'shell', zsh: 'shell', sql: 'sql', php: 'php',
    rb: 'ruby', swift: 'swift', kt: 'kotlin', dart: 'dart',
  };
  return map[ext] || 'plaintext';
}

function fileLang(filePath: string): string {
  const ext = filePath.split('.').pop()?.toLowerCase() || '';
  return extToMonacoLang(ext);
}

export function CodeEditor({ filePath, content, language, apiBaseUrl, onSave }: CodeEditorProps) {
  const [dirty, setDirty] = useState(false);
  const [saving, setSaving] = useState(false);
  const editorRef = useRef<Parameters<OnMount>[0] | null>(null);
  const monacoRef = useRef<Parameters<OnMount>[1] | null>(null);
  const contentRef = useRef(content);
  const filePathRef = useRef(filePath);

  // Keep refs in sync
  useEffect(() => { contentRef.current = content; }, [content]);
  useEffect(() => { filePathRef.current = filePath; }, [filePath]);

  // Reset dirty when file changes
  useEffect(() => { setDirty(false); }, [filePath]);

  const handleEditorMount: OnMount = (editor, monaco) => {
    editorRef.current = editor;
    monacoRef.current = monaco;

    // Ctrl+S / Cmd+S to save
    editor.addCommand(monaco.KeyMod.CtrlCmd | monaco.KeyCode.KeyS, async () => {
      const currentContent = editor.getValue();
      setSaving(true);
      try {
        await onSave(filePathRef.current, currentContent);
        setDirty(false);
      } finally {
        setSaving(false);
      }
    });
  };

  const handleChange = (value: string | undefined) => {
    if (value !== undefined && value !== contentRef.current) {
      setDirty(true);
    }
  };

  const handleSaveClick = async () => {
    if (!editorRef.current) return;
    const currentContent = editorRef.current.getValue();
    setSaving(true);
    try {
      await onSave(filePath, currentContent);
      setDirty(false);
    } finally {
      setSaving(false);
    }
  };

  const fileName = filePath.split(/[/\\]/).pop() || filePath;
  // 始终通过文件路径扩展名映射 Monaco 语言 ID，
  // 避免后端返回的原始扩展名（如 "ts"、"tsx"）不被 Monaco 识别
  const lang = fileLang(filePath);

  return (
    <div className="ce-container">
      <div className="ce-header">
        <div className="ce-tabs">
          <span className="ce-tab active">
            <span className="ce-tab-icon">📄</span>
            <span>{fileName}</span>
            {dirty && <span className="ce-dirty">●</span>}
          </span>
        </div>
        <div className="ce-actions">
          {dirty && (
            <button className="ce-save-btn" onClick={handleSaveClick} disabled={saving}>
              {saving ? 'Saving...' : 'Save'}
            </button>
          )}
        </div>
      </div>
      <div className="ce-editor-wrapper">
        <Editor
          key={filePath}
          defaultLanguage={lang}
          language={lang}
          theme="vs-dark"
          value={content}
          onChange={handleChange}
          onMount={handleEditorMount}
          options={{
            fontSize: 14,
            fontFamily: "'Cascadia Code', 'Fira Code', 'JetBrains Mono', Consolas, monospace",
            minimap: { enabled: true },
            scrollBeyondLastLine: false,
            lineNumbers: 'on',
            renderWhitespace: 'selection',
            tabSize: 2,
            wordWrap: 'off',
            automaticLayout: true,
            bracketPairColorization: { enabled: true },
            padding: { top: 8 },
          }}
        />
      </div>
    </div>
  );
}
