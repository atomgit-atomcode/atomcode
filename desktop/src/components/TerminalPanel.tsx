import { useEffect, useRef, useState } from 'react';

interface TerminalPanelProps {
  apiBaseUrl: string;
  cwd: string;
}

interface TerminalLine {
  text: string;
  type: 'input' | 'output' | 'error' | 'system';
}

export function TerminalPanel({ apiBaseUrl, cwd }: TerminalPanelProps) {
  const [lines, setLines] = useState<TerminalLine[]>([
    { text: 'AtomCode IDE Terminal', type: 'system' },
    { text: 'Type commands to execute in the project directory.', type: 'system' },
    { text: '', type: 'output' },
  ]);
  const [input, setInput] = useState('');
  const [executing, setExecuting] = useState(false);
  const [history, setHistory] = useState<string[]>([]);
  const [historyIdx, setHistoryIdx] = useState(-1);
  const [minimized, setMinimized] = useState(false);
  const terminalRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLInputElement>(null);

  // Auto-scroll
  useEffect(() => {
    if (terminalRef.current) {
      terminalRef.current.scrollTop = terminalRef.current.scrollHeight;
    }
  }, [lines]);

  const addLine = (text: string, type: TerminalLine['type'] = 'output') => {
    setLines((prev) => [...prev, { text, type }]);
  };

  const execute = async (cmd: string) => {
    if (!cmd.trim()) return;
    addLine(`$ ${cmd}`, 'input');
    setInput('');
    setHistory((prev) => [...prev, cmd]);
    setHistoryIdx(-1);
    setExecuting(true);

    try {
      // Use the daemon's bash tool via the live message endpoint
      const resp = await fetch(`${apiBaseUrl}/live/message`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          text: cmd,
          working_dir: cwd,
        }),
      });

      if (resp.ok) {
        addLine('Command sent to AtomCode agent.');
        addLine('(Full terminal integration coming soon — use the daemon tools)');
      } else {
        const text = await resp.text();
        addLine(`Error: ${resp.status} ${text}`, 'error');
      }
    } catch (err: any) {
      addLine(`Error: ${err.message}`, 'error');
    } finally {
      setExecuting(false);
    }
  };

  const handleKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === 'Enter') {
      e.preventDefault();
      execute(input);
    } else if (e.key === 'ArrowUp') {
      e.preventDefault();
      if (history.length > 0) {
        const idx = historyIdx === -1 ? history.length - 1 : Math.max(0, historyIdx - 1);
        setHistoryIdx(idx);
        setInput(history[idx]);
      }
    } else if (e.key === 'ArrowDown') {
      e.preventDefault();
      if (historyIdx >= 0) {
        const idx = historyIdx + 1;
        if (idx >= history.length) {
          setHistoryIdx(-1);
          setInput('');
        } else {
          setHistoryIdx(idx);
          setInput(history[idx]);
        }
      }
    }
  };

  if (minimized) {
    return (
      <div className="term-bar" onClick={() => setMinimized(false)}>
        <span className="term-bar-text">Terminal</span>
        <span className="term-bar-action">▲</span>
      </div>
    );
  }

  return (
    <div className="term-container">
      <div className="term-header">
        <span className="term-title">TERMINAL</span>
        <div className="term-actions">
          <button className="term-btn" onClick={() => setLines([{ text: 'Terminal cleared', type: 'system' }])}>
            Clear
          </button>
          <button className="term-btn" onClick={() => setMinimized(true)}>
            ▼
          </button>
        </div>
      </div>
      <div className="term-body" ref={terminalRef}>
        {lines.map((line, i) => (
          <div key={i} className={`term-line term-line-${line.type}`}>
            {line.text}
          </div>
        ))}
        {executing && <div className="term-line term-line-output term-cursor">▋</div>}
      </div>
      <div className="term-input-row">
        <span className="term-prompt">$</span>
        <input
          ref={inputRef}
          className="term-input"
          value={input}
          onChange={(e) => setInput(e.target.value)}
          onKeyDown={handleKeyDown}
          disabled={executing}
          placeholder="Type a command..."
          spellCheck={false}
          autoComplete="off"
        />
      </div>
    </div>
  );
}
