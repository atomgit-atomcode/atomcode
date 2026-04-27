(function() {
  const vscode = acquireVsCodeApi();
  const messagesEl = document.getElementById('messages');
  const inputEl = document.getElementById('input');
  const btnSend = document.getElementById('btn-send');
  const btnStop = document.getElementById('btn-stop');
  const btnNew = document.getElementById('btn-new');
  const generatingIndicator = document.getElementById('generating-indicator');
  const tokenCount = document.getElementById('token-count');

  let isGenerating = false;
  let currentAssistantEl = null;
  let currentTextBuffer = '';

  // Tell extension we're ready
  vscode.postMessage({ type: 'ready' });

  // --- Input handling ---
  inputEl.addEventListener('keydown', function(e) {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      sendMessage();
    }
  });
  btnSend.addEventListener('click', sendMessage);
  btnStop.addEventListener('click', function() { vscode.postMessage({ type: 'stop' }); });
  btnNew.addEventListener('click', function() { vscode.postMessage({ type: 'newConversation' }); });

  // Auto-resize textarea
  inputEl.addEventListener('input', function() {
    inputEl.style.height = 'auto';
    inputEl.style.height = Math.min(inputEl.scrollHeight, 150) + 'px';
  });

  function sendMessage() {
    var text = inputEl.value.trim();
    if (!text || isGenerating) return;
    addMessage('user', text);
    vscode.postMessage({ type: 'send', text: text });
    inputEl.value = '';
    inputEl.style.height = 'auto';
  }

  // --- Message rendering ---
  function addMessage(role, content) {
    var msgEl = document.createElement('div');
    msgEl.className = 'message message-' + role;

    var roleEl = document.createElement('div');
    roleEl.className = 'message-role';
    roleEl.textContent = role === 'user' ? 'You' : 'AtomCode';

    var contentEl = document.createElement('div');
    contentEl.className = 'message-content';

    if (role === 'user') {
      contentEl.textContent = content;
    } else {
      contentEl.innerHTML = renderMarkdown(content);
    }

    msgEl.appendChild(roleEl);
    msgEl.appendChild(contentEl);
    messagesEl.appendChild(msgEl);
    scrollToBottom();
    return contentEl;
  }

  function addToolCall(name, args) {
    var toolEl = document.createElement('div');
    toolEl.className = 'tool-call';

    var headerEl = document.createElement('div');
    headerEl.className = 'tool-call-header';
    headerEl.addEventListener('click', function() {
      toolEl.classList.toggle('expanded');
    });

    var iconSpan = document.createElement('span');
    iconSpan.className = 'tool-icon';
    iconSpan.textContent = '🔧';

    var nameSpan = document.createElement('span');
    nameSpan.className = 'tool-name';
    nameSpan.textContent = name;

    var argsSpan = document.createElement('span');
    argsSpan.className = 'tool-args';
    argsSpan.textContent = formatToolArgs(name, args);

    var statusSpan = document.createElement('span');
    statusSpan.className = 'tool-status';

    headerEl.appendChild(iconSpan);
    headerEl.appendChild(nameSpan);
    headerEl.appendChild(argsSpan);
    headerEl.appendChild(statusSpan);

    var bodyEl = document.createElement('div');
    bodyEl.className = 'tool-call-body';
    bodyEl.textContent = args;

    toolEl.appendChild(headerEl);
    toolEl.appendChild(bodyEl);

    if (currentAssistantEl) {
      currentAssistantEl.appendChild(toolEl);
    } else {
      messagesEl.appendChild(toolEl);
    }
    scrollToBottom();
    return toolEl;
  }

  var currentToolEl = null;

  function updateToolResult(name, output, success, durationMs) {
    if (currentToolEl) {
      var statusEl = currentToolEl.querySelector('.tool-status');
      var secs = (durationMs / 1000).toFixed(1);
      statusEl.textContent = success ? ('✓ ' + secs + 's') : ('✗ ' + secs + 's');
      statusEl.className = 'tool-status ' + (success ? 'success' : 'failure');

      var bodyEl = currentToolEl.querySelector('.tool-call-body');
      bodyEl.textContent = output.substring(0, 2000);
      if (output.length > 2000) bodyEl.textContent += '\n... (truncated)';

      currentToolEl = null;
    }
    scrollToBottom();
  }

  // --- Markdown rendering (simple) ---
  function renderMarkdown(text) {
    var html = escapeHtml(text);

    // Code blocks
    html = html.replace(/```(\w*)\n([\s\S]*?)```/g, function(_, lang, code) {
      return '<pre><code class="language-' + lang + '">' + code + '</code></pre>';
    });

    // Inline code
    html = html.replace(/`([^`]+)`/g, '<code>$1</code>');

    // Bold
    html = html.replace(/\*\*([^*]+)\*\*/g, '<strong>$1</strong>');

    // Italic
    html = html.replace(/\*([^*]+)\*/g, '<em>$1</em>');

    // Line breaks
    html = html.replace(/\n/g, '<br>');

    return html;
  }

  function escapeHtml(text) {
    var div = document.createElement('div');
    div.textContent = text;
    return div.innerHTML;
  }

  function formatToolArgs(name, argsJson) {
    try {
      var args = JSON.parse(argsJson);
      if (name === 'read_file' || name === 'write_file' || name === 'edit_file') {
        return args.file_path || '';
      }
      if (name === 'bash') return (args.command || '').substring(0, 60);
      if (name === 'grep') return (args.pattern || '') + ' in ' + (args.path || '.');
      return '';
    } catch (e) { return ''; }
  }

  function scrollToBottom() {
    requestAnimationFrame(function() {
      messagesEl.scrollTop = messagesEl.scrollHeight;
    });
  }

  // --- Handle messages from extension ---
  window.addEventListener('message', function(event) {
    var msg = event.data;
    switch (msg.type) {
      case 'init':
        isGenerating = msg.generating;
        updateUI();
        break;

      case 'userMessage':
        addMessage('user', msg.text);
        break;

      case 'generationStarted':
        isGenerating = true;
        currentTextBuffer = '';
        currentAssistantEl = addMessage('assistant', '');
        updateUI();
        break;

      case 'text':
        currentTextBuffer += msg.content;
        if (currentAssistantEl) {
          currentAssistantEl.innerHTML = renderMarkdown(currentTextBuffer);
        }
        scrollToBottom();
        break;

      case 'toolStart':
        currentToolEl = addToolCall(msg.name, msg.args);
        break;

      case 'toolResult':
        updateToolResult(msg.name, msg.output, msg.success, msg.durationMs);
        break;

      case 'tokens':
        tokenCount.textContent = (msg.total / 1000).toFixed(1) + 'k tokens';
        break;

      case 'done':
        isGenerating = false;
        currentAssistantEl = null;
        currentTextBuffer = '';
        updateUI();
        break;

      case 'stopped':
      case 'error':
        isGenerating = false;
        if (msg.type === 'error' && msg.message) {
          addMessage('assistant', 'Error: ' + msg.message);
        }
        currentAssistantEl = null;
        currentTextBuffer = '';
        updateUI();
        break;

      case 'generationStopped':
        isGenerating = false;
        updateUI();
        break;

      case 'clearChat':
        messagesEl.innerHTML = '';
        tokenCount.textContent = '';
        break;

      case 'focusInput':
        inputEl.focus();
        break;
    }
  });

  function updateUI() {
    generatingIndicator.classList.toggle('hidden', !isGenerating);
    btnSend.disabled = isGenerating;
    inputEl.disabled = isGenerating;
  }
})();
