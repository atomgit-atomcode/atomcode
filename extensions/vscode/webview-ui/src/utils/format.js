"use strict";
Object.defineProperty(exports, "__esModule", { value: true });
exports.formatTokenCount = formatTokenCount;
exports.formatTimeAgo = formatTimeAgo;
exports.escapeHtml = escapeHtml;
exports.groupSessionsByDate = groupSessionsByDate;
exports.getToolIcon = getToolIcon;
exports.formatToolArgs = formatToolArgs;
/** Format a token count for display (e.g. 1234 -> "1.2k") */
function formatTokenCount(total) {
    if (total < 1000)
        return `${total} tokens`;
    return `${(total / 1000).toFixed(1)}k tokens`;
}
/** Human-readable time-ago string */
function formatTimeAgo(dateStr) {
    if (!dateStr)
        return '';
    const diff = Date.now() - new Date(dateStr).getTime();
    const mins = Math.floor(diff / 60000);
    if (mins < 1)
        return 'just now';
    if (mins < 60)
        return `${mins}m ago`;
    const hours = Math.floor(mins / 60);
    if (hours < 24)
        return `${hours}h ago`;
    const days = Math.floor(hours / 24);
    return `${days}d ago`;
}
/** Escape HTML entities */
function escapeHtml(text) {
    return text
        .replace(/&/g, '&amp;')
        .replace(/</g, '&lt;')
        .replace(/>/g, '&gt;')
        .replace(/"/g, '&quot;')
        .replace(/'/g, '&#39;');
}
/** Group sessions by date category */
function groupSessionsByDate(sessions) {
    const groups = {};
    const now = Date.now();
    const oneDay = 86400000;
    sessions.forEach((s) => {
        const ts = new Date(s.updated_at ?? s.created_at ?? now).getTime();
        const diff = now - ts;
        let label;
        if (diff < oneDay)
            label = 'Today';
        else if (diff < 2 * oneDay)
            label = 'Yesterday';
        else if (diff < 7 * oneDay)
            label = 'This Week';
        else
            label = 'Older';
        if (!groups[label])
            groups[label] = [];
        groups[label].push(s);
    });
    return groups;
}
/** Get a display-friendly icon for a tool name */
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
    return icons[name] ?? '🔧';
}
/** Format tool args into a short summary */
function formatToolArgs(name, argsJson) {
    try {
        const args = JSON.parse(argsJson);
        if (name === 'read_file' || name === 'write_file' || name === 'edit_file') {
            return args.file_path ?? args.path ?? '';
        }
        if (name === 'bash')
            return (args.command ?? '').substring(0, 80);
        if (name === 'grep')
            return `${args.pattern ?? ''} in ${args.path ?? '.'}`;
        if (name === 'list_dir')
            return args.path ?? '.';
        return '';
    }
    catch {
        return '';
    }
}
//# sourceMappingURL=format.js.map