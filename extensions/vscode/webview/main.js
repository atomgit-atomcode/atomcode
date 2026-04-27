/* ===========================================================
   AtomCode Webview — Main Client Logic
   =========================================================== */
(function () {
  'use strict';

  // ----- VS Code API -----
  const vscode = acquireVsCodeApi();

  // ----- DOM references -----
  const $  = (s) => document.querySelector(s);
  const $$ = (s) => document.querySelectorAll(s);

  const dom = {
    app:               $('#app'),
    header:            $('#header'),
    btnHistory:        $('#btn-history'),
    btnNew:            $('#btn-new'),
    btnSettings:       $('#btn-settings'),
    btnPopout:         $('#btn-popout'),
    btnModel:          $('#btn-model'),
    currentModelLabel: $('#current-model-label'),
    modelDropdown:     $('#model-dropdown'),
    modelList:         $('#model-list'),
    historyPanel:      $('#history-panel'),
    historySearchInput:$('#history-search-input'),
    historyList:       $('#history-list'),
    mainContent:       $('#main-content'),
    welcomeScreen:     $('#welcome-screen'),
    messages:          $('#messages'),
    generatingIndicator: $('#generating-indicator'),
    generatingStatus:  $('#generating-status'),
    btnStop:           $('#btn-stop'),
    contextTags:       $('#context-tags'),
    input:             $('#input'),
    btnSend:           $('#btn-send'),
    btnAttach:         $('#btn-attach'),
    tokenCount:        $('#token-count'),
    slashPicker:       $('#slash-picker'),
    slashList:         $('#slash-list'),
    mentionPicker:     $('#mention-picker'),
    mentionList:       $('#mention-list'),
  };

  // ----- State -----
  const state = {
    isGenerating: false,
    hasMessages: false,
    currentModel: 'default',
    models: [],
    sessions: [],
    contextFiles: [],         // { path, language?, lines? }
    currentAssistantEl: null,
    currentTextBuffer: '',
    currentToolEl: null,
    slashPickerActive: false,
    slashPickerIndex: 0,
    mentionPickerActive: false,
    historyOpen: false,
    modelDropdownOpen: false,
    autoScroll: true,
  };

  // ----- Slash commands definition -----
  const SLASH_COMMANDS = [
    { cmd: '/explain',  desc: 'Explain selected code' },
    { cmd: '/fix',      desc: 'Fix bugs in code' },
    { cmd: '/test',     desc: 'Generate unit tests' },
    { cmd: '/review',   desc: 'Code review' },
    { cmd: '/docs',     desc: 'Generate documentation' },
    { cmd: '/refactor', desc: 'Refactor code' },
    { cmd: '/optimize', desc: 'Optimize performance' },
  ];

  // ----- Quick action map -----
  const QUICK_ACTIONS = {
    explain:  '/explain ',
    fix:      '/fix ',
    test:     '/test ',
    review:   '/review ',
    refactor: '/refactor ',
    docs:     '/docs ',
  };

  // =================================================================
  //  INITIALIZATION
  // =================================================================
  function init() {
    bindEvents();
    populateSlashPicker(SLASH_COMMANDS);
    vscode.postMessage({ type: 'ready' });
  }

  // =================================================================
  //  EVENT BINDING
  // =================================================================
  function bindEvents() {
    // Header buttons
    dom.btnHistory.addEventListener('click', toggleHistory);
    dom.btnNew.addEventListener('click', () => {
      vscode.postMessage({ type: 'newConversation' });
    });
    dom.btnSettings.addEventListener('click', () => {
      vscode.postMessage({ type: 'openSettings' });
    });
    dom.btnPopout.addEventListener('click', () => {
      vscode.postMessage({ type: 'popout' });
    });
    dom.btnModel.addEventListener('click', toggleModelDropdown);
    dom.btnStop.addEventListener('click', () => {
      vscode.postMessage({ type: 'stop' });
    });
    dom.btnAttach.addEventListener('click', () => {
      vscode.postMessage({ type: 'attachFile' });
    });

    // Send button
    dom.btnSend.addEventListener('click', sendMessage);

    // Input handling
    dom.input.addEventListener('keydown', handleInputKeydown);
    dom.input.addEventListener('input', handleInputChange);

    // Welcome card clicks
    $$('.welcome-card').forEach((card) => {
      card.addEventListener('click', () => {
        const action = card.dataset.action;
        if (QUICK_ACTIONS[action]) {
          dom.input.value = QUICK_ACTIONS[action];
          dom.input.focus();
          updateSendButton();
          hideWelcomeScreen();
          vscode.postMessage({ type: 'quickAction', action });
        }
      });
    });

    // Welcome quick command buttons
    $$('.welcome-quick-btn').forEach((btn) => {
      btn.addEventListener('click', () => {
        const cmd = btn.dataset.command;
        dom.input.value = cmd + ' ';
        dom.input.focus();
        updateSendButton();
        hideWelcomeScreen();
      });
    });

    // History search
    dom.historySearchInput.addEventListener('input', filterHistoryList);

    // Smart scroll detection
    dom.mainContent.addEventListener('scroll', () => {
      const el = dom.mainContent;
      const threshold = 60;
      state.autoScroll = (el.scrollHeight - el.scrollTop - el.clientHeight) < threshold;
    });

    // Close dropdowns on outside click
    document.addEventListener('click', (e) => {
      if (state.modelDropdownOpen && !dom.modelDropdown.contains(e.target) && !dom.btnModel.contains(e.target)) {
        closeModelDropdown();
      }
      if (state.historyOpen && !dom.historyPanel.contains(e.target) && !dom.btnHistory.contains(e.target)) {
        closeHistory();
      }
    });

    // Global escape handler
    document.addEventListener('keydown', (e) => {
      if (e.key === 'Escape') {
        if (state.slashPickerActive) { hideSlashPicker(); return; }
        if (state.mentionPickerActive) { hideMentionPicker(); return; }
        if (state.modelDropdownOpen) { closeModelDropdown(); return; }
        if (state.historyOpen) { closeHistory(); return; }
      }
    });

    // Handle messages from extension
    window.addEventListener('message', handleExtensionMessage);
  }

  // =================================================================
  //  INPUT HANDLING
  // =================================================================
  function handleInputKeydown(e) {
    // Slash picker navigation
    if (state.slashPickerActive) {
      if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
        e.preventDefault();
        navigateSlashPicker(e.key === 'ArrowDown' ? 1 : -1);
        return;
      }
      if (e.key === 'Enter' || e.key === 'Tab') {
        e.preventDefault();
        selectSlashItem(state.slashPickerIndex);
        return;
      }
    }

    // Send on Enter (without Shift)
    if (e.key === 'Enter' && !e.shiftKey && !e.isComposing) {
      e.preventDefault();
      sendMessage();
    }
  }

  function handleInputChange() {
    autoResizeTextarea();
    updateSendButton();
    handleSlashTrigger();
  }

  function autoResizeTextarea() {
    dom.input.style.height = 'auto';
    dom.input.style.height = Math.min(dom.input.scrollHeight, 160) + 'px';
  }

  function updateSendButton() {
    const hasContent = dom.input.value.trim().length > 0;
    dom.btnSend.disabled = !hasContent || state.isGenerating;
  }

  // =================================================================
  //  SLASH COMMAND PICKER
  // =================================================================
  function handleSlashTrigger() {
    const val = dom.input.value;
    // Show picker if input starts with /
    if (val.startsWith('/') && !val.includes(' ')) {
      const query = val.slice(1).toLowerCase();
      const filtered = SLASH_COMMANDS.filter(
        (c) => c.cmd.slice(1).startsWith(query)
      );
      if (filtered.length > 0) {
        populateSlashPicker(filtered);
        showSlashPicker();
        return;
      }
    }
    hideSlashPicker();
  }

  function populateSlashPicker(commands) {
    dom.slashList.innerHTML = '';
    commands.forEach((c, i) => {
      const item = document.createElement('div');
      item.className = 'slash-item' + (i === 0 ? ' active' : '');
      item.dataset.index = i;
      item.innerHTML =
        '<span class="slash-item-cmd">' + escapeHtml(c.cmd) + '</span>' +
        '<span class="slash-item-desc">' + escapeHtml(c.desc) + '</span>';
      item.addEventListener('click', () => selectSlashItem(i));
      item.addEventListener('mouseenter', () => {
        setSlashPickerIndex(i);
      });
      dom.slashList.appendChild(item);
    });
    state.slashPickerIndex = 0;
  }

  function showSlashPicker() {
    dom.slashPicker.classList.remove('hidden');
    state.slashPickerActive = true;
  }
  function hideSlashPicker() {
    dom.slashPicker.classList.add('hidden');
    state.slashPickerActive = false;
  }

  function navigateSlashPicker(dir) {
    const items = dom.slashList.querySelectorAll('.slash-item');
    if (!items.length) return;
    let idx = state.slashPickerIndex + dir;
    if (idx < 0) idx = items.length - 1;
    if (idx >= items.length) idx = 0;
    setSlashPickerIndex(idx);
  }

  function setSlashPickerIndex(idx) {
    const items = dom.slashList.querySelectorAll('.slash-item');
    items.forEach((el, i) => el.classList.toggle('active', i === idx));
    state.slashPickerIndex = idx;
    // Scroll into view
    if (items[idx]) items[idx].scrollIntoView({ block: 'nearest' });
  }

  function selectSlashItem(idx) {
    const items = dom.slashList.querySelectorAll('.slash-item');
    if (!items[idx]) return;
    const cmd = items[idx].querySelector('.slash-item-cmd').textContent;
    dom.input.value = cmd + ' ';
    dom.input.focus();
    hideSlashPicker();
    updateSendButton();
  }

  // =================================================================
  //  MENTION PICKER (placeholder for @ mentions)
  // =================================================================
  function hideMentionPicker() {
    dom.mentionPicker.classList.add('hidden');
    state.mentionPickerActive = false;
  }

  // =================================================================
  //  MODEL SELECTOR
  // =================================================================
  function toggleModelDropdown(e) {
    e && e.stopPropagation();
    if (state.modelDropdownOpen) {
      closeModelDropdown();
    } else {
      openModelDropdown();
    }
  }
  function openModelDropdown() {
    dom.modelDropdown.classList.remove('hidden');
    state.modelDropdownOpen = true;
  }
  function closeModelDropdown() {
    dom.modelDropdown.classList.add('hidden');
    state.modelDropdownOpen = false;
  }

  function renderModelList(models, currentModel) {
    dom.modelList.innerHTML = '';
    if (!models || !models.length) {
      const empty = document.createElement('div');
      empty.className = 'dropdown-item';
      empty.textContent = 'No models available';
      empty.style.opacity = '0.6';
      dom.modelList.appendChild(empty);
      return;
    }
    models.forEach((model) => {
      const name = typeof model === 'string' ? model : model.model || model.provider || model.name || String(model);
      const item = document.createElement('div');
      item.className = 'dropdown-item' + (name === currentModel ? ' active' : '');
      item.textContent = name;
      item.addEventListener('click', () => {
        state.currentModel = name;
        dom.currentModelLabel.textContent = name;
        closeModelDropdown();
        vscode.postMessage({ type: 'selectModel', model: name });
        renderModelList(state.models, name);
      });
      dom.modelList.appendChild(item);
    });
  }

  // =================================================================
  //  HISTORY PANEL
  // =================================================================
  function toggleHistory() {
    if (state.historyOpen) {
      closeHistory();
    } else {
      openHistory();
    }
  }
  function openHistory() {
    dom.historyPanel.classList.remove('hidden');
    // Trigger reflow then add visible class for animation
    void dom.historyPanel.offsetWidth;
    dom.historyPanel.classList.add('visible');
    state.historyOpen = true;
    dom.historySearchInput.focus();
  }
  function closeHistory() {
    dom.historyPanel.classList.remove('visible');
    state.historyOpen = false;
    // After transition, hide
    setTimeout(() => {
      if (!state.historyOpen) {
        dom.historyPanel.classList.add('hidden');
      }
    }, 220);
  }

  function renderHistoryList(sessions) {
    dom.historyList.innerHTML = '';
    if (!sessions || !sessions.length) {
      dom.historyList.innerHTML = '<div class="history-empty">No session history</div>';
      return;
    }

    const grouped = groupSessionsByDate(sessions);
    for (const [label, items] of Object.entries(grouped)) {
      const groupLabel = document.createElement('div');
      groupLabel.className = 'history-group-label';
      groupLabel.textContent = label;
      dom.historyList.appendChild(groupLabel);

      items.forEach((s) => {
        const item = document.createElement('div');
        item.className = 'history-item';
        item.innerHTML =
          '<span class="history-item-title">' + escapeHtml(s.name || s.title || 'Untitled') + '</span>' +
          '<span class="history-item-time">' + formatTimeAgo(s.updated_at || s.updatedAt || s.created_at || s.createdAt) + '</span>';
        item.addEventListener('click', () => {
          vscode.postMessage({ type: 'loadSession', sessionId: s.id });
          closeHistory();
        });
        dom.historyList.appendChild(item);
      });
    }
  }

  function filterHistoryList() {
    const query = dom.historySearchInput.value.toLowerCase().trim();
    if (!query) {
      renderHistoryList(state.sessions);
      return;
    }
    const filtered = state.sessions.filter(
      (s) => (s.name || s.title || '').toLowerCase().includes(query)
    );
    renderHistoryList(filtered);
  }

  function groupSessionsByDate(sessions) {
    const groups = {};
    const now = Date.now();
    const oneDay = 86400000;

    sessions.forEach((s) => {
      const ts = new Date(s.updated_at || s.updatedAt || s.created_at || s.createdAt || now).getTime();
      const diff = now - ts;
      let label;
      if (diff < oneDay) label = 'Today';
      else if (diff < 2 * oneDay) label = 'Yesterday';
      else if (diff < 7 * oneDay) label = 'This Week';
      else label = 'Older';
      if (!groups[label]) groups[label] = [];
      groups[label].push(s);
    });
    return groups;
  }

  function formatTimeAgo(dateStr) {
    if (!dateStr) return '';
    const diff = Date.now() - new Date(dateStr).getTime();
    const mins = Math.floor(diff / 60000);
    if (mins < 1) return 'just now';
    if (mins < 60) return mins + 'm ago';
    const hours = Math.floor(mins / 60);
    if (hours < 24) return hours + 'h ago';
    const days = Math.floor(hours / 24);
    return days + 'd ago';
  }

  // =================================================================
  //  CONTEXT TAGS
  // =================================================================
  function addContextTag(filePath, type) {
    // Avoid duplicates
    if (state.contextFiles.some((f) => f.path === filePath)) return;
    state.contextFiles.push({ path: filePath, type: type || 'file' });
    renderContextTags();
  }

  function removeContextTag(filePath) {
    state.contextFiles = state.contextFiles.filter((f) => f.path !== filePath);
    renderContextTags();
  }

  function renderContextTags() {
    dom.contextTags.innerHTML = '';
    if (state.contextFiles.length === 0) {
      dom.contextTags.classList.add('hidden');
      return;
    }
    dom.contextTags.classList.remove('hidden');
    state.contextFiles.forEach((f) => {
      const tag = document.createElement('span');
      tag.className = 'context-tag';
      const icon = f.type === 'selection' ? '🔍' : '📄';
      const name = f.path.split('/').pop();
      tag.innerHTML =
        '<span class="context-tag-icon">' + icon + '</span>' +
        '<span>' + escapeHtml(name) + '</span>' +
        '<button class="context-tag-remove" title="Remove">×</button>';
      tag.querySelector('.context-tag-remove').addEventListener('click', () => {
        removeContextTag(f.path);
      });
      dom.contextTags.appendChild(tag);
    });
  }

  // =================================================================
  //  WELCOME SCREEN
  // =================================================================
  function showWelcomeScreen() {
    dom.welcomeScreen.classList.remove('hidden');
    dom.messages.classList.add('hidden');
  }
  function hideWelcomeScreen() {
    dom.welcomeScreen.classList.add('hidden');
    dom.messages.classList.remove('hidden');
    state.hasMessages = true;
  }

  // =================================================================
  //  SENDING MESSAGES
  // =================================================================
  function sendMessage() {
    const text = dom.input.value.trim();
    if (!text || state.isGenerating) return;

    // Check if slash command
    const slashMatch = text.match(/^\/(\w+)\s*(.*)/);
    if (slashMatch) {
      const knownCmd = SLASH_COMMANDS.find((c) => c.cmd === '/' + slashMatch[1]);
      if (knownCmd) {
        vscode.postMessage({ type: 'slashCommand', command: slashMatch[1], args: slashMatch[2] });
      }
    }

    hideWelcomeScreen();
    addUserMessage(text);

    const context = state.contextFiles.length > 0
      ? state.contextFiles.map((f) => ({ path: f.path, type: f.type }))
      : undefined;

    vscode.postMessage({ type: 'send', text: text, context: context });

    dom.input.value = '';
    dom.input.style.height = 'auto';
    updateSendButton();
  }

  // =================================================================
  //  MESSAGE RENDERING
  // =================================================================
  function addUserMessage(text) {
    const msg = document.createElement('div');
    msg.className = 'message message-user';

    const header = document.createElement('div');
    header.className = 'message-header';
    const roleEl = document.createElement('span');
    roleEl.className = 'message-role';
    roleEl.textContent = 'You';
    header.appendChild(roleEl);

    const content = document.createElement('div');
    content.className = 'message-content';
    content.textContent = text;

    msg.appendChild(header);
    msg.appendChild(content);

    // Append context chips if present
    if (state.contextFiles.length > 0) {
      const chips = document.createElement('div');
      chips.className = 'message-context';
      state.contextFiles.forEach((f) => {
        const chip = document.createElement('span');
        chip.className = 'context-chip';
        const icon = f.type === 'selection' ? '🔍' : '📄';
        const name = f.path.split('/').pop();
        chip.innerHTML = icon + ' ' + escapeHtml(name);
        chip.addEventListener('click', () => {
          vscode.postMessage({ type: 'openFile', path: f.path });
        });
        chips.appendChild(chip);
      });
      content.appendChild(chips);
    }

    dom.messages.appendChild(msg);
    scrollToBottom();
  }

  function createAssistantMessage() {
    const msg = document.createElement('div');
    msg.className = 'message message-assistant';

    const header = document.createElement('div');
    header.className = 'message-header';
    const roleEl = document.createElement('span');
    roleEl.className = 'message-role';
    roleEl.textContent = 'AtomCode';
    header.appendChild(roleEl);

    const content = document.createElement('div');
    content.className = 'message-content';

    msg.appendChild(header);
    msg.appendChild(content);
    dom.messages.appendChild(msg);
    scrollToBottom();
    return content;
  }

  function addErrorMessage(text) {
    const msg = document.createElement('div');
    msg.className = 'message message-error';

    const content = document.createElement('div');
    content.className = 'message-content';
    content.textContent = text;

    msg.appendChild(content);
    dom.messages.appendChild(msg);
    scrollToBottom();
  }

  // =================================================================
  //  TOOL CALLS
  // =================================================================
  function addToolCall(name, argsJson) {
    const container = document.createElement('div');
    container.className = 'tool-call';

    const header = document.createElement('div');
    header.className = 'tool-call-header';
    header.addEventListener('click', () => {
      container.classList.toggle('expanded');
    });

    // Chevron
    const chevron = document.createElement('svg');
    chevron.setAttribute('class', 'tool-call-chevron');
    chevron.setAttribute('width', '14');
    chevron.setAttribute('height', '14');
    chevron.setAttribute('viewBox', '0 0 14 14');
    chevron.setAttribute('fill', 'currentColor');
    chevron.innerHTML = '<path d="M5 2.5l4.5 4.5L5 11.5" stroke="currentColor" stroke-width="1.3" fill="none" stroke-linecap="round" stroke-linejoin="round"/>';

    // Icon
    const icon = document.createElement('span');
    icon.className = 'tool-call-icon';
    icon.textContent = getToolIcon(name);

    // Name
    const nameEl = document.createElement('span');
    nameEl.className = 'tool-call-name';
    nameEl.textContent = name;

    // Args summary
    const argsEl = document.createElement('span');
    argsEl.className = 'tool-call-args';
    argsEl.textContent = formatToolArgs(name, argsJson);

    // Status (spinner while running)
    const statusEl = document.createElement('div');
    statusEl.className = 'tool-call-spinner';

    header.appendChild(chevron);
    header.appendChild(icon);
    header.appendChild(nameEl);
    header.appendChild(argsEl);
    header.appendChild(statusEl);

    // Body
    const body = document.createElement('div');
    body.className = 'tool-call-body';
    const bodyContent = document.createElement('div');
    bodyContent.className = 'tool-call-body-content';
    bodyContent.textContent = argsJson || '';
    body.appendChild(bodyContent);

    container.appendChild(header);
    container.appendChild(body);

    // Insert into the assistant message or messages root
    if (state.currentAssistantEl) {
      // Insert after the last element in the assistant message's parent
      state.currentAssistantEl.closest('.message-assistant').appendChild(container);
    } else {
      dom.messages.appendChild(container);
    }
    scrollToBottom();
    return container;
  }

  function updateToolResult(name, output, success, durationMs) {
    if (!state.currentToolEl) return;

    const header = state.currentToolEl.querySelector('.tool-call-header');
    // Replace spinner with status text
    const spinner = header.querySelector('.tool-call-spinner');
    if (spinner) {
      const statusEl = document.createElement('span');
      statusEl.className = 'tool-call-status ' + (success ? 'success' : 'failure');
      const secs = (durationMs / 1000).toFixed(1);
      statusEl.textContent = (success ? '✓ ' : '✗ ') + secs + 's';
      spinner.replaceWith(statusEl);
    }

    // Update body
    const bodyContent = state.currentToolEl.querySelector('.tool-call-body-content');
    if (bodyContent) {
      const truncated = output.length > 3000 ? output.substring(0, 3000) + '\n... (truncated)' : output;
      bodyContent.textContent = truncated;
    }

    // Update generating status
    dom.generatingStatus.textContent = '';

    state.currentToolEl = null;
    scrollToBottom();
  }

  function getToolIcon(name) {
    const icons = {
      read_file: '📄',
      write_file: '✍️',
      edit_file: '✍️',
      bash: '💻',
      grep: '🔍',
      list_dir: '📁',
      search: '🔍',
    };
    return icons[name] || '🔧';
  }

  function formatToolArgs(name, argsJson) {
    try {
      const args = JSON.parse(argsJson);
      if (name === 'read_file' || name === 'write_file' || name === 'edit_file') {
        return args.file_path || args.path || '';
      }
      if (name === 'bash') return (args.command || '').substring(0, 80);
      if (name === 'grep') return (args.pattern || '') + ' in ' + (args.path || '.');
      if (name === 'list_dir') return args.path || '.';
      return '';
    } catch (e) { return ''; }
  }

  // =================================================================
  //  MARKDOWN RENDERING
  // =================================================================
  function renderMarkdown(text) {
    if (!text) return '';

    // Split into code blocks and non-code-block segments
    const segments = [];
    let remaining = text;
    const codeBlockRegex = /```(\w*)\n([\s\S]*?)```/g;
    let match;
    let lastIndex = 0;

    while ((match = codeBlockRegex.exec(remaining)) !== null) {
      // Text before code block
      if (match.index > lastIndex) {
        segments.push({ type: 'text', content: remaining.slice(lastIndex, match.index) });
      }
      segments.push({ type: 'code', lang: match[1], content: match[2] });
      lastIndex = match.index + match[0].length;
    }
    // Trailing text
    if (lastIndex < remaining.length) {
      segments.push({ type: 'text', content: remaining.slice(lastIndex) });
    }

    let html = '';
    segments.forEach((seg) => {
      if (seg.type === 'code') {
        html += renderCodeBlock(seg.lang, seg.content);
      } else {
        html += renderInlineMarkdown(seg.content);
      }
    });

    return html;
  }

  function renderCodeBlock(lang, code) {
    const langLabel = lang || 'code';
    const escapedCode = escapeHtml(code.replace(/\n$/, ''));
    const id = 'cb-' + Math.random().toString(36).slice(2, 8);

    return (
      '<div class="code-block-wrapper" data-code-id="' + id + '">' +
        '<div class="code-block-header">' +
          '<span class="code-block-lang">' + escapeHtml(langLabel) + '</span>' +
          '<div class="code-block-actions">' +
            '<button class="code-action-btn" data-action="copy" title="Copy code">Copy</button>' +
            '<button class="code-action-btn" data-action="apply" title="Apply to file">Apply</button>' +
            '<button class="code-action-btn" data-action="insert" title="Insert at cursor">Insert</button>' +
          '</div>' +
        '</div>' +
        '<pre><code class="language-' + escapeHtml(lang) + '">' + escapedCode + '</code></pre>' +
      '</div>'
    );
  }

  function renderInlineMarkdown(text) {
    let html = escapeHtml(text);

    // Headers (must come before other processing)
    html = html.replace(/^#### (.+)$/gm, '<h4>$1</h4>');
    html = html.replace(/^### (.+)$/gm, '<h3>$1</h3>');
    html = html.replace(/^## (.+)$/gm, '<h2>$1</h2>');
    html = html.replace(/^# (.+)$/gm, '<h1>$1</h1>');

    // Horizontal rule
    html = html.replace(/^---$/gm, '<hr>');

    // Blockquote
    html = html.replace(/^&gt; (.+)$/gm, '<blockquote>$1</blockquote>');

    // Unordered lists (simple one-level)
    html = html.replace(/^[\s]*[-*] (.+)$/gm, '<li>$1</li>');
    html = html.replace(/((?:<li>.*<\/li>\n?)+)/g, '<ul>$1</ul>');

    // Ordered lists
    html = html.replace(/^[\s]*\d+\. (.+)$/gm, '<li>$1</li>');

    // Bold
    html = html.replace(/\*\*([^*]+)\*\*/g, '<strong>$1</strong>');

    // Italic
    html = html.replace(/\*([^*]+)\*/g, '<em>$1</em>');

    // Inline code
    html = html.replace(/`([^`]+)`/g, '<code>$1</code>');

    // Links [text](url)
    html = html.replace(/\[([^\]]+)\]\(([^)]+)\)/g, '<a href="$2" title="$2">$1</a>');

    // Paragraphs: convert double newlines
    html = html.replace(/\n\n/g, '</p><p>');

    // Single newlines -> <br>
    html = html.replace(/\n/g, '<br>');

    // Wrap in paragraph if not already wrapped in block element
    if (html && !html.startsWith('<h') && !html.startsWith('<ul') && !html.startsWith('<ol') && !html.startsWith('<hr')) {
      html = '<p>' + html + '</p>';
    }

    return html;
  }

  // =================================================================
  //  CODE BLOCK ACTIONS (delegated)
  // =================================================================
  dom.messages.addEventListener('click', (e) => {
    const btn = e.target.closest('.code-action-btn');
    if (!btn) return;

    const wrapper = btn.closest('.code-block-wrapper');
    if (!wrapper) return;

    const codeEl = wrapper.querySelector('pre code');
    if (!codeEl) return;

    const code = codeEl.textContent;
    const lang = (codeEl.className.match(/language-(\w+)/) || [])[1] || '';
    const action = btn.dataset.action;

    if (action === 'copy') {
      vscode.postMessage({ type: 'copyCode', code: code });
      // Also try native clipboard
      navigator.clipboard.writeText(code).catch(() => {});
      btn.textContent = 'Copied!';
      btn.classList.add('copied');
      setTimeout(() => {
        btn.textContent = 'Copy';
        btn.classList.remove('copied');
      }, 2000);
    } else if (action === 'apply') {
      vscode.postMessage({ type: 'applyCode', code: code, language: lang });
    } else if (action === 'insert') {
      vscode.postMessage({ type: 'insertCode', code: code, language: lang });
    }
  });

  // Also handle context-chip clicks in messages
  dom.messages.addEventListener('click', (e) => {
    const chip = e.target.closest('.context-chip');
    if (chip && chip.dataset.path) {
      vscode.postMessage({ type: 'openFile', path: chip.dataset.path });
    }
  });

  // =================================================================
  //  SCROLLING
  // =================================================================
  function scrollToBottom() {
    if (!state.autoScroll) return;
    requestAnimationFrame(() => {
      dom.mainContent.scrollTop = dom.mainContent.scrollHeight;
    });
  }

  // =================================================================
  //  UI STATE
  // =================================================================
  function setGenerating(generating) {
    state.isGenerating = generating;
    dom.generatingIndicator.classList.toggle('hidden', !generating);
    dom.btnSend.disabled = generating || !dom.input.value.trim();
    dom.input.disabled = generating;
    if (!generating) {
      dom.input.focus();
    }
  }

  // =================================================================
  //  EXTENSION MESSAGE HANDLER
  // =================================================================
  function handleExtensionMessage(event) {
    const msg = event.data;
    switch (msg.type) {

      case 'init':
        setGenerating(msg.generating || false);
        if (msg.models) {
          state.models = msg.models;
          renderModelList(msg.models, msg.currentModel || state.currentModel);
        }
        if (msg.currentModel) {
          state.currentModel = msg.currentModel;
          dom.currentModelLabel.textContent = msg.currentModel;
        }
        break;

      case 'userMessage':
        hideWelcomeScreen();
        addUserMessage(msg.text);
        break;

      case 'generationStarted':
        hideWelcomeScreen();
        setGenerating(true);
        state.currentTextBuffer = '';
        state.currentAssistantEl = createAssistantMessage();
        break;

      case 'text':
        state.currentTextBuffer += msg.content;
        if (state.currentAssistantEl) {
          state.currentAssistantEl.innerHTML = renderMarkdown(state.currentTextBuffer);
          // Add streaming cursor
          appendStreamingCursor(state.currentAssistantEl);
        }
        scrollToBottom();
        break;

      case 'toolStart':
        state.currentToolEl = addToolCall(msg.name, msg.args);
        // Update status text
        var toolLabel = formatToolArgs(msg.name, msg.args);
        dom.generatingStatus.textContent = getToolIcon(msg.name) + ' ' + msg.name + (toolLabel ? ': ' + toolLabel : '');
        break;

      case 'toolResult':
        updateToolResult(msg.name, msg.output, msg.success, msg.durationMs);
        break;

      case 'tokens':
        if (msg.total) {
          dom.tokenCount.textContent = (msg.total / 1000).toFixed(1) + 'k tokens';
        }
        break;

      case 'done':
        setGenerating(false);
        removeStreamingCursor();
        if (msg.tokens && msg.tokens.total) {
          dom.tokenCount.textContent = (msg.tokens.total / 1000).toFixed(1) + 'k tokens';
        }
        state.currentAssistantEl = null;
        state.currentTextBuffer = '';
        state.currentToolEl = null;
        break;

      case 'stopped':
        setGenerating(false);
        removeStreamingCursor();
        state.currentAssistantEl = null;
        state.currentTextBuffer = '';
        state.currentToolEl = null;
        break;

      case 'error':
        setGenerating(false);
        removeStreamingCursor();
        if (msg.message) {
          addErrorMessage('Error: ' + msg.message);
        }
        state.currentAssistantEl = null;
        state.currentTextBuffer = '';
        state.currentToolEl = null;
        break;

      case 'generationStopped':
        setGenerating(false);
        removeStreamingCursor();
        break;

      case 'clearChat':
        dom.messages.innerHTML = '';
        dom.tokenCount.textContent = '';
        state.hasMessages = false;
        state.currentAssistantEl = null;
        state.currentTextBuffer = '';
        state.currentToolEl = null;
        state.contextFiles = [];
        renderContextTags();
        showWelcomeScreen();
        break;

      case 'focusInput':
        dom.input.focus();
        break;

      case 'sessions':
        state.sessions = msg.sessions || [];
        renderHistoryList(state.sessions);
        break;

      case 'models':
        state.models = msg.models || [];
        renderModelList(state.models, state.currentModel);
        break;

      case 'context':
        if (msg.filePath) {
          addContextTag(msg.filePath, msg.selection ? 'selection' : 'file');
        }
        break;
    }
  }

  // =================================================================
  //  STREAMING CURSOR
  // =================================================================
  function appendStreamingCursor(el) {
    // Remove any existing cursor first
    removeStreamingCursor();
    const cursor = document.createElement('span');
    cursor.className = 'streaming-cursor';
    cursor.id = 'streaming-cursor';

    // Find the deepest last text-containing element
    const lastBlock = findLastTextNode(el);
    if (lastBlock) {
      lastBlock.appendChild(cursor);
    } else {
      el.appendChild(cursor);
    }
  }

  function removeStreamingCursor() {
    const existing = document.getElementById('streaming-cursor');
    if (existing) existing.remove();
  }

  function findLastTextNode(el) {
    // Walk backwards through children to find the last element that can hold text
    const children = el.children;
    if (children.length === 0) return el;
    for (let i = children.length - 1; i >= 0; i--) {
      const child = children[i];
      // Skip code-block-wrapper as cursor shouldn't go there
      if (child.classList && child.classList.contains('code-block-wrapper')) continue;
      if (child.classList && child.classList.contains('tool-call')) continue;
      if (child.tagName === 'P' || child.tagName === 'LI' || child.tagName === 'SPAN' ||
          child.tagName === 'STRONG' || child.tagName === 'EM' || child.tagName === 'BLOCKQUOTE' ||
          child.tagName === 'H1' || child.tagName === 'H2' || child.tagName === 'H3' || child.tagName === 'H4') {
        return findLastTextNode(child);
      }
      return child;
    }
    return el;
  }

  // =================================================================
  //  UTILITIES
  // =================================================================
  function escapeHtml(text) {
    const div = document.createElement('div');
    div.textContent = text;
    return div.innerHTML;
  }

  // =================================================================
  //  START
  // =================================================================
  init();
})();
