// Shared behavior for docs pages: theme toggle, mobile sidebar, icon sync, search.
(function () {
  function getTheme() { return document.documentElement.getAttribute('data-theme') || 'dark'; }
  function syncIcon() {
    var icon = document.getElementById('themeIcon');
    if (!icon) return;
    icon.textContent = getTheme() === 'dark' ? '\u263C' : '\u263E'; // ☼ / ☾
  }
  window.toggleDocsTheme = function () {
    var next = getTheme() === 'dark' ? 'light' : 'dark';
    document.documentElement.setAttribute('data-theme', next);
    try { localStorage.setItem('atomcode-theme', next); } catch (e) {}
    syncIcon();
  };
  document.addEventListener('DOMContentLoaded', function () {
    syncIcon();
    var toggle = document.getElementById('sidebarToggle');
    var sidebar = document.getElementById('docsSidebar');
    if (toggle && sidebar) {
      toggle.addEventListener('click', function () { sidebar.classList.toggle('open'); });
    }
    initSearch();
  });

  // ── Client-side docs search ──
  var searchIndex = null; // [{url, pageTitle, heading, text, raw}]

  var DOC_PAGES = [
    { url: 'index.html',                title: '概览' },
    { url: 'getting-started.html',      title: '快速开始' },
    { url: 'configuration.html',        title: '配置文件' },
    { url: 'login.html',                title: '登录方式' },
    { url: 'basic-usage.html',          title: '基本使用' },
    { url: 'slash-commands.html',       title: '斜杠命令' },
    { url: 'keybindings.html',          title: '快捷键' },
    { url: 'tools.html',               title: '内置工具' },
    { url: 'sessions.html',            title: '会话与撤销' },
    { url: 'project-instructions.html', title: '项目指令文件' },
    { url: 'memory.html',              title: '永久记忆' },
    { url: 'skills.html',              title: 'Skills 扩展' },
    { url: 'plugins.html',             title: 'Plugin 系统' },
    { url: 'mcp.html',                 title: 'MCP 集成' },
    { url: 'faq.html',                 title: '常见问题' },
  ];

  function initSearch() {
    var container = document.getElementById('docsSearch');
    if (!container) return;

    container.innerHTML =
      '<div class="docs-search-wrap">' +
        '<input id="searchInput" type="text" placeholder="搜索文档… (⌘K)" autocomplete="off" />' +
        '<div id="searchResults" class="docs-search-results" style="display:none"></div>' +
      '</div>';

    var input = document.getElementById('searchInput');
    var results = document.getElementById('searchResults');
    var debounce = null;

    input.addEventListener('input', function () {
      clearTimeout(debounce);
      debounce = setTimeout(function () { runSearch(input.value.trim(), results); }, 200);
    });

    input.addEventListener('focus', function () {
      if (input.value.trim().length >= 1) runSearch(input.value.trim(), results);
    });

    document.addEventListener('click', function (e) {
      if (!container.contains(e.target)) results.style.display = 'none';
    });

    document.addEventListener('keydown', function (e) {
      if ((e.metaKey || e.ctrlKey) && e.key === 'k') {
        e.preventDefault();
        input.focus();
        input.select();
      }
      if (e.key === 'Escape') {
        results.style.display = 'none';
        input.blur();
      }
    });

    // Pre-build index on idle so first search is instant
    if (window.requestIdleCallback) {
      window.requestIdleCallback(function () { buildIndex(function () {}); });
    } else {
      setTimeout(function () { buildIndex(function () {}); }, 1000);
    }
  }

  // Resolve URL relative to current page's directory
  function resolveUrl(rel) {
    // Get the directory of the current page
    var base = window.location.href;
    var lastSlash = base.lastIndexOf('/');
    if (lastSlash >= 0) base = base.substring(0, lastSlash + 1);
    return base + rel;
  }

  function buildIndex(callback) {
    if (searchIndex) { callback(searchIndex); return; }

    // Strategy 1: fetch pages via HTTP (works on http:// servers)
    // Strategy 2: if fetch fails (file://), index the current page only
    var useCurrentPageOnly = window.location.protocol === 'file:';

    if (useCurrentPageOnly) {
      searchIndex = indexCurrentPage();
      callback(searchIndex);
      return;
    }

    searchIndex = [];
    var remaining = DOC_PAGES.length;

    DOC_PAGES.forEach(function (page) {
      fetch(resolveUrl(page.url))
        .then(function (r) {
          if (!r.ok) throw new Error(r.status);
          return r.text();
        })
        .then(function (html) {
          var entries = parsePageHtml(html, page.url, page.title);
          searchIndex = searchIndex.concat(entries);
        })
        .catch(function () {
          // Page fetch failed — skip silently
        })
        .finally(function () {
          remaining--;
          if (remaining <= 0) callback(searchIndex);
        });
    });
  }

  // Index just the current page (file:// fallback)
  function indexCurrentPage() {
    var main = document.querySelector('.prose-docs') || document.querySelector('main');
    if (!main) return [];
    var title = document.title.split('·')[0].trim() || '文档';
    var url = window.location.pathname.split('/').pop() || 'index.html';
    return extractSections(main, url, title);
  }

  // Parse fetched HTML string into search entries
  function parsePageHtml(html, url, pageTitle) {
    var parser = new DOMParser();
    var doc = parser.parseFromString(html, 'text/html');
    var main = doc.querySelector('.prose-docs') || doc.querySelector('main') || doc.body;
    return extractSections(main, url, pageTitle);
  }

  // Extract sections from a DOM element, split by h2/h3
  function extractSections(root, url, pageTitle) {
    var entries = [];
    var headings = root.querySelectorAll('h1, h2, h3');

    if (headings.length === 0) {
      // No headings — index the whole page as one entry
      var text = (root.textContent || '').replace(/\s+/g, ' ').trim();
      if (text) {
        entries.push({
          url: url,
          pageTitle: pageTitle,
          heading: pageTitle,
          text: text.toLowerCase(),
          raw: text,
        });
      }
      return entries;
    }

    // Walk through all elements, grouping text by heading
    var currentHeading = pageTitle;
    var currentText = '';

    function flush() {
      var cleaned = currentText.replace(/\s+/g, ' ').trim();
      if (cleaned) {
        entries.push({
          url: url,
          pageTitle: pageTitle,
          heading: currentHeading,
          text: cleaned.toLowerCase(),
          raw: cleaned,
        });
      }
    }

    // Collect text between headings using TreeWalker for deep text extraction
    var allElements = root.querySelectorAll('*');
    var headingSet = new Set(headings);
    var seenText = new Set();

    for (var i = 0; i < allElements.length; i++) {
      var el = allElements[i];
      if (headingSet.has(el)) {
        flush();
        currentHeading = (el.textContent || '').replace(/\s+/g, ' ').trim();
        currentText = '';
      } else if (el.children.length === 0 || el.tagName === 'TD' || el.tagName === 'LI' || el.tagName === 'P' || el.tagName === 'CODE') {
        // Leaf nodes or content containers — grab their text
        var t = (el.textContent || '').trim();
        if (t && !seenText.has(t)) {
          seenText.add(t);
          currentText += ' ' + t;
        }
      }
    }
    flush();

    return entries;
  }

  function runSearch(query, resultsEl) {
    if (query.length < 1) { resultsEl.style.display = 'none'; return; }

    buildIndex(function (index) {
      var q = query.toLowerCase();
      var keywords = q.split(/\s+/).filter(function (w) { return w.length > 0; });
      if (keywords.length === 0) { resultsEl.style.display = 'none'; return; }

      var matches = [];
      index.forEach(function (entry) {
        var score = 0;
        var matchAll = keywords.every(function (kw) {
          var inText = entry.text.indexOf(kw) !== -1;
          var inHeading = entry.heading.toLowerCase().indexOf(kw) !== -1;
          if (inHeading) score += 10;
          if (inText) score += 1;
          return inText || inHeading;
        });
        if (!matchAll) return;

        var snippet = extractSnippet(entry.raw, keywords, 50);
        matches.push({
          url: entry.url,
          pageTitle: entry.pageTitle,
          heading: entry.heading,
          snippet: snippet,
          score: score,
        });
      });

      // Sort by relevance (heading matches first)
      matches.sort(function (a, b) { return b.score - a.score; });

      if (matches.length === 0) {
        resultsEl.innerHTML = '<div class="search-empty">没有找到 "' + escHtml(query) + '" 相关结果</div>';
        resultsEl.style.display = 'block';
        return;
      }

      // Deduplicate by url+heading
      var seen = {};
      var unique = [];
      matches.forEach(function (m) {
        var key = m.url + '#' + m.heading;
        if (!seen[key]) { seen[key] = true; unique.push(m); }
      });

      var html = unique.slice(0, 10).map(function (m) {
        var highlighted = highlightSnippet(m.snippet, keywords);
        return '<a class="search-item" href="' + escHtml(m.url) + '">' +
          '<span class="search-page">' + escHtml(m.pageTitle) + '</span>' +
          '<span class="search-heading">' + escHtml(m.heading) + '</span>' +
          '<span class="search-snippet">' + highlighted + '</span>' +
        '</a>';
      }).join('');

      resultsEl.innerHTML = html;
      resultsEl.style.display = 'block';
    });
  }

  function extractSnippet(raw, keywords, radius) {
    var lower = raw.toLowerCase();
    var pos = -1;
    for (var i = 0; i < keywords.length; i++) {
      pos = lower.indexOf(keywords[i]);
      if (pos !== -1) break;
    }
    if (pos === -1) return raw.substring(0, radius * 2);
    var start = Math.max(0, pos - radius);
    var end = Math.min(raw.length, pos + radius * 2);
    var snippet = raw.substring(start, end);
    if (start > 0) snippet = '…' + snippet;
    if (end < raw.length) snippet += '…';
    return snippet;
  }

  function highlightSnippet(text, keywords) {
    var escaped = escHtml(text);
    keywords.forEach(function (kw) {
      var re = new RegExp('(' + escRegex(escHtml(kw)) + ')', 'gi');
      escaped = escaped.replace(re, '<mark>$1</mark>');
    });
    return escaped;
  }

  function escHtml(s) { return s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;'); }
  function escRegex(s) { return s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'); }
})();
