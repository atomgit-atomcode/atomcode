import { useCallback, useEffect, useRef, useState } from 'react';
import { streamChat, SSEEvent, getModels, ModelInfo, createSession, listSessions, getSkills, SkillInfo } from '../api/daemonClient';

interface ChatPanelProps {
  apiBaseUrl: string;
  cwd: string;
  onClose: () => void;
}

interface Message {
  role: 'user' | 'assistant' | 'system';
  content: string;
  isStreaming?: boolean;
}

export function ChatPanel({ apiBaseUrl, cwd, onClose }: ChatPanelProps) {
  const [messages, setMessages] = useState<Message[]>([]);
  const [input, setInput] = useState('');
  const [streaming, setStreaming] = useState(false);
  const [sessionId, setSessionId] = useState<string | null>(null);
  const [models, setModels] = useState<ModelInfo[]>([]);
  const [selectedModel, setSelectedModel] = useState<string>('');
  const [skills, setSkills] = useState<SkillInfo[]>([]);
  const [showSkills, setShowSkills] = useState(false);
  const messagesEndRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLTextAreaElement>(null);
  const abortRef = useRef<AbortController | null>(null);

  // Load models & skills on mount
  useEffect(() => {
    getModels(apiBaseUrl).then((ms) => {
      setModels(ms);
      if (ms.length > 0 && !selectedModel) {
        setSelectedModel(`${ms[0].provider}/${ms[0].model}`);
      }
    }).catch(() => {});
    getSkills(apiBaseUrl).then(setSkills).catch(() => {});
  }, [apiBaseUrl]);

  // Auto-scroll to bottom
  useEffect(() => {
    messagesEndRef.current?.scrollIntoView({ behavior: 'smooth' });
  }, [messages]);

  // Auto-focus input
  useEffect(() => {
    inputRef.current?.focus();
  }, []);

  const sendMessage = useCallback(async () => {
    const text = input.trim();
    if (!text || streaming) return;
    setInput('');

    // Create a session if needed
    let sid = sessionId;
    if (!sid) {
      try {
        const session = await createSession(apiBaseUrl, cwd);
        sid = session.id;
        setSessionId(sid);
      } catch (err) {
        console.error('Failed to create session:', err);
      }
    }

    const userMsg: Message = { role: 'user', content: text };
    const assistantMsg: Message = { role: 'assistant', content: '', isStreaming: true };

    setMessages((prev) => [...prev, userMsg, assistantMsg]);
    setStreaming(true);

    const controller = new AbortController();
    abortRef.current = controller;

    try {
      await streamChat(
        apiBaseUrl,
        { message: text, working_dir: cwd, session_id: sid ?? undefined },
        (event: SSEEvent) => {
          switch (event.type) {
            case 'text':
              setMessages((prev) => {
                const last = prev[prev.length - 1];
                if (last?.isStreaming) {
                  const updated = [...prev];
                  updated[updated.length - 1] = {
                    ...last,
                    content: last.content + event.data,
                  };
                  return updated;
                }
                return prev;
              });
              break;
            case 'error':
              setMessages((prev) => [
                ...prev.slice(0, -1),
                { role: 'assistant', content: `Error: ${event.data.message}`, isStreaming: false },
              ]);
              break;
            case 'done':
              setMessages((prev) => {
                const updated = [...prev];
                const last = updated[updated.length - 1];
                if (last?.isStreaming) {
                  updated[updated.length - 1] = { ...last, isStreaming: false };
                }
                return updated;
              });
              if (event.data.session_id) {
                setSessionId(event.data.session_id);
              }
              break;
          }
        },
        controller.signal,
      );
    } catch (err: any) {
      if (err.name !== 'AbortError') {
        setMessages((prev) => [
          ...prev.slice(0, -1),
          { role: 'assistant', content: `Error: ${err.message}`, isStreaming: false },
        ]);
      }
    } finally {
      setStreaming(false);
      abortRef.current = null;
    }
  }, [input, streaming, sessionId, apiBaseUrl, cwd]);

  const handleStop = () => {
    abortRef.current?.abort();
    setStreaming(false);
    setMessages((prev) => {
      const updated = [...prev];
      const last = updated[updated.length - 1];
      if (last?.isStreaming) {
        updated[updated.length - 1] = { ...last, isStreaming: false };
      }
      return updated;
    });
  };

  const handleKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      sendMessage();
    }
  };

  const insertSkill = (name: string) => {
    setInput(`/${name} `);
    setShowSkills(false);
    inputRef.current?.focus();
  };

  return (
    <div className="chat-panel">
      <div className="chat-header">
        <span className="chat-title">AI Chat</span>
        <div className="chat-header-right">
          {models.length > 0 && (
            <select
              className="chat-model-select"
              value={selectedModel}
              onChange={(e) => setSelectedModel(e.target.value)}
            >
              {models.map((m) => (
                <option key={`${m.provider}/${m.model}`} value={`${m.provider}/${m.model}`}>
                  {m.provider}/{m.model}
                </option>
              ))}
            </select>
          )}
          <button className="chat-close-btn" onClick={onClose}>✕</button>
        </div>
      </div>

      <div className="chat-messages">
        {messages.length === 0 && (
          <div className="chat-welcome">
            <div className="chat-welcome-icon">💎</div>
            <div className="chat-welcome-title">AtomCode AI</div>
            <div className="chat-welcome-text">
              Ask me to read, edit, or analyze your code. I can run commands and make changes autonomously.
            </div>
            {skills.length > 0 && (
              <div className="chat-skills-list">
                {skills.slice(0, 6).map((s) => (
                  <button key={s.name} className="chat-skill-chip" onClick={() => insertSkill(s.name)}>
                    /{s.name}
                  </button>
                ))}
              </div>
            )}
          </div>
        )}
        {messages.map((msg, i) => (
          <div key={i} className={`chat-msg chat-msg-${msg.role}`}>
            <div className="chat-msg-content">
              {msg.content || (msg.isStreaming ? <span className="chat-cursor">▋</span> : '')}
              {msg.isStreaming && msg.content && <span className="chat-cursor">▋</span>}
            </div>
          </div>
        ))}
        <div ref={messagesEndRef} />
      </div>

      <div className="chat-input-area">
        {showSkills && skills.length > 0 && (
          <div className="chat-skills-popup">
            {skills.map((s) => (
              <button key={s.name} className="chat-skills-item" onClick={() => insertSkill(s.name)}>
                <span className="css-name">/{s.name}</span>
                <span className="css-desc">{s.description}</span>
              </button>
            ))}
          </div>
        )}
        <div className="chat-input-row">
          <textarea
            ref={inputRef}
            className="chat-input"
            value={input}
            onChange={(e) => setInput(e.target.value)}
            onKeyDown={handleKeyDown}
            placeholder="Ask AtomCode to do something... (Enter to send, Shift+Enter for new line)"
            rows={2}
            disabled={streaming}
          />
          <div className="chat-input-actions">
            {skills.length > 0 && (
              <button className="chat-action-btn" onClick={() => setShowSkills(!showSkills)} title="Skills">
                /
              </button>
            )}
            {streaming ? (
              <button className="chat-stop-btn" onClick={handleStop}>
                ■
              </button>
            ) : (
              <button className="chat-send-btn" onClick={sendMessage} disabled={!input.trim()}>
                ➤
              </button>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}
