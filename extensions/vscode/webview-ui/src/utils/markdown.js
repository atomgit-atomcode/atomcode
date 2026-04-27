"use strict";
Object.defineProperty(exports, "__esModule", { value: true });
exports.renderMarkdown = renderMarkdown;
const format_1 = require("./format");
/**
 * Lightweight Markdown -> HTML renderer.
 * Handles fenced code blocks, inline code, bold, italic, headers,
 * lists, blockquotes, links, and line breaks.
 */
function renderMarkdown(text) {
    if (!text)
        return '';
    // Split into code blocks and text segments
    const segments = [];
    const codeBlockRegex = /```(\w*)\n([\s\S]*?)```/g;
    let lastIndex = 0;
    let match;
    while ((match = codeBlockRegex.exec(text)) !== null) {
        if (match.index > lastIndex) {
            segments.push({ type: 'text', content: text.slice(lastIndex, match.index) });
        }
        segments.push({ type: 'code', lang: match[1], content: match[2] });
        lastIndex = match.index + match[0].length;
    }
    if (lastIndex < text.length) {
        segments.push({ type: 'text', content: text.slice(lastIndex) });
    }
    return segments
        .map((seg) => (seg.type === 'code' ? renderCodeBlock(seg.lang ?? '', seg.content) : renderInlineMarkdown(seg.content)))
        .join('');
}
function renderCodeBlock(lang, code) {
    const langLabel = lang || 'code';
    const escapedCode = (0, format_1.escapeHtml)(code.replace(/\n$/, ''));
    const id = `cb-${Math.random().toString(36).slice(2, 8)}`;
    return (`<div class="code-block-wrapper" data-code-id="${id}">` +
        `<div class="code-block-header">` +
        `<span class="code-block-lang">${(0, format_1.escapeHtml)(langLabel)}</span>` +
        `<div class="code-block-actions">` +
        `<button class="code-action-btn" data-action="copy" title="Copy code">Copy</button>` +
        `<button class="code-action-btn" data-action="apply" title="Apply to file">Apply</button>` +
        `<button class="code-action-btn" data-action="insert" title="Insert at cursor">Insert</button>` +
        `</div></div>` +
        `<pre><code class="language-${(0, format_1.escapeHtml)(lang)}">${escapedCode}</code></pre>` +
        `</div>`);
}
function renderInlineMarkdown(text) {
    let html = (0, format_1.escapeHtml)(text);
    // Headers
    html = html.replace(/^#### (.+)$/gm, '<h4>$1</h4>');
    html = html.replace(/^### (.+)$/gm, '<h3>$1</h3>');
    html = html.replace(/^## (.+)$/gm, '<h2>$1</h2>');
    html = html.replace(/^# (.+)$/gm, '<h1>$1</h1>');
    // Horizontal rule
    html = html.replace(/^---$/gm, '<hr>');
    // Blockquote
    html = html.replace(/^&gt; (.+)$/gm, '<blockquote>$1</blockquote>');
    // Unordered lists
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
    // Links
    html = html.replace(/\[([^\]]+)\]\(([^)]+)\)/g, '<a href="$2" title="$2">$1</a>');
    // Paragraphs
    html = html.replace(/\n\n/g, '</p><p>');
    html = html.replace(/\n/g, '<br>');
    if (html &&
        !html.startsWith('<h') &&
        !html.startsWith('<ul') &&
        !html.startsWith('<ol') &&
        !html.startsWith('<hr')) {
        html = `<p>${html}</p>`;
    }
    return html;
}
//# sourceMappingURL=markdown.js.map