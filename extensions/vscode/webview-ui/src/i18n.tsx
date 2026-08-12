import React, { createContext, useContext, useEffect, useMemo } from 'react';

export type Lang = 'zh' | 'en';
type TParams = Record<string, string | number | boolean>;

const zh = {
  'app.productName': 'AtomCode',

  'welcome.subtitle.ready': 'AI 编程助手',
  'welcome.subtitle.setup': '设置 AtomCode，开始在 VS Code 中对话',
  'welcome.quick.intro': '了解 AtomCode',
  'welcome.quick.projectOverview': '项目概览',
  'welcome.quick.improvements': '查找改进点',
  'welcome.quick.devPlan': '生成开发计划',
  'welcome.quick.configuration': '配置帮助',
  'welcome.quick.tips': '使用技巧',

  'setup.account': '账号',
  'setup.signedInAs': '已登录为 {name}',
  'setup.atomgitUser': 'AtomGit 用户',
  'setup.signInHint': '登录后可使用 AtomGit CodingPlan 模型。',
  'setup.refreshAccount': '刷新账号',
  'setup.signInWithAtomGit': '使用 AtomGit 登录',
  'setup.copy': '复制',
  'setup.cancel': '取消',
  'setup.models': '模型',
  'setup.providersConfigured': '已配置 {count} 个 Provider',
  'setup.syncOrAddProvider': '同步 CodingPlan 模型，或手动添加 Provider。',
  'setup.syncCodingPlanModels': '同步 CodingPlan 模型',
  'setup.addProviderManually': '手动添加 Provider',
  'setup.providerName': 'Provider 名称',
  'setup.providerType': '类型，例如 openai',
  'setup.model': '模型',
  'setup.baseUrl': 'Base URL',
  'setup.apiKey': 'API key',
  'setup.saveProvider': '保存 Provider',
  'setup.waitingForBrowser': '等待浏览器授权…',
  'setup.signedInNextStep': '已登录。请同步 CodingPlan 模型或添加 Provider。',

  'header.openSessions': '打开会话侧栏',
  'header.newConversation': '新建会话',
  'header.search': '搜索',

  'input.imageTooLarge': '图片必须小于 {mb} MB。',
  'input.insertPath': '插入路径',
  'input.chooseFile': '选择文件',
  'input.searchWorkspace': '搜索工作区',
  'input.uploadImage': '上传图片',
  'input.searchProjectFiles': '搜索项目文件…',
  'input.noMatchingFiles': '没有匹配文件',
  'input.typeToSearchFiles': '输入以搜索工作区文件',
  'input.folder': '文件夹',
  'input.dismiss': '关闭',
  'input.removeImage': '移除图片',
  'input.placeholder': '输入消息…',
  'input.commands': '命令',
  'input.attachFile': '附加文件',
  'input.queueMessage': '消息排队',
  'input.stop': '停止',
  'input.send': '发送',

  'session.select': '选择',
  'session.done': '完成',
  'session.close': '关闭',
  'session.new': '新建会话',
  'session.searchPlaceholder': '搜索会话…',
  'session.selectedCount': '已选 {count} 项',
  'session.deleteSelected': '删除所选',
  'session.cancelSelection': '取消选择',
  'session.empty': '暂无会话',
  'session.untitled': '未命名',
  'session.rename': '修改名称',
  'session.delete': '删除会话',
  'session.today': '今天',
  'session.yesterday': '昨天',
  'session.thisWeek': '本周',
  'session.older': '更早',

  'search.placeholder': '搜索消息…',
  'search.found': '找到 {count} 条',
  'search.position': '{current}/{total}',
  'search.noResults': '无结果',
  'search.prev': '上一个',
  'search.next': '下一个',
  'search.close': '关闭搜索',
  'search.scrollLatest': '滚动到最新',

  'model.effortTitle': '推理强度',
  'model.effortLabel': '强度',
  'model.effortDefault': '默认',
  'model.selectModel': '选择模型',
  'model.noModels': '暂无可用模型',
  'model.defaultBadge': '默认',
  'model.loginRequired': '需登录',
  'mode.title': '审批模式',
  'mode.build': 'Build',
  'mode.buildDesc': '改动前逐个审批',
  'mode.acceptEdits': 'Accept Edits',
  'mode.acceptEditsDesc': '自动批准编辑，命令需审批',
  'mode.auto': 'Auto',
  'mode.autoDesc': '自动批准所有操作',
  'mode.plan': 'Plan',
  'mode.planDesc': '只读探索，先出方案',

  'slash.login': '登录并同步 CodingPlan 模型',
  'slash.logout': '退出 AtomGit 登录',
  'slash.whoami': '显示当前登录用户',
  'slash.status': '显示会话状态',
  'slash.config': '显示配置路径',
  'slash.reload': '从磁盘重新加载配置',
  'slash.skill': 'Skill',

  'tool.copied': '已复制',
  'tool.copy': '复制 {label}',
  'tool.input': '输入',
  'tool.output': '输出',
  'tool.waiting': '等待',
  'tool.error': '错误',
  'tool.applied': '已应用',
  'tool.done': '完成',
  'tool.destructive': '破坏性操作',
  'tool.waitingApproval': '待审批',
  'tool.incomplete': '未完成',

  'todo.title': '任务',
  'todo.progress': '{completed}/{total} 已完成',
  'todo.pending': '待处理',
  'todo.inProgress': '进行中',
  'todo.completed': '已完成',
  'todo.expand': '展开任务清单',
  'todo.collapse': '收起任务清单',

  'stream.rateLimitedRetrying': '触发限流，{seconds} 秒后重试…',
  'stream.rateLimitedPaused': '触发限流，等待恢复…',
  'stream.idleNotice': '仍在等待模型响应…',
  'stream.incomplete': '本轮未完成（{reason}）。已保留当前结果，可在新一轮中继续。',

  'permission.deny': '拒绝',
  'permission.allow': '允许',
  'permission.alwaysAllow': '始终允许',
  'permission.allowPersist': '允许并持久化',
  'permission.denying': '拒绝中...',
  'permission.allowing': '允许中...',
  'permission.alwaysAllowing': '始终允许中...',
  'permission.persisting': '持久化中...',

  'assistant.artifact': '产物',
  'assistant.streaming': '生成中',
  'assistant.copied': '已复制',
  'assistant.copy': '复制',
  'user.queued': '排队中',
  'user.selection': '选区',
  'user.file': '文件',
  'user.imageUnavailable': '图片不可用',
  'user.expand': '展开',
  'user.collapse': '收起',

  'image.preview': '图片预览',
  'image.previous': '上一张',
  'image.next': '下一张',
  'image.closePreview': '关闭预览',

  'provider.settingsTitle': 'AtomCode 设置',
  'provider.notSignedIn': '未登录',
  'provider.providers': 'Providers',
  'provider.noneConfigured': '未配置 Provider。',
  'provider.keySet': '已设置 key',
  'provider.use': '使用',
  'provider.thinkOn': '思考开启',
  'provider.thinkOff': '思考关闭',
  'provider.delete': '删除',
  'provider.refresh': '刷新',
  'provider.addProvider': '添加 Provider',
  'provider.typePlaceholder': '类型，例如 openai',
  'provider.saveProvider': '保存 Provider',

  'time.justNow': '刚刚',
  'time.minutesAgo': '{count} 分钟前',
  'time.hoursAgo': '{count} 小时前',
  'time.daysAgo': '{count} 天前',
  'time.older': '更早',
  'token.count': '{count} tokens',
  'token.countK': '{count}k tokens',
} as const;

export type MsgKey = keyof typeof zh;

const en: Record<MsgKey, string> = {
  'app.productName': 'AtomCode',

  'welcome.subtitle.ready': 'AI-powered coding assistant',
  'welcome.subtitle.setup': 'Set up AtomCode to start chatting in VS Code',
  'welcome.quick.intro': 'Learn AtomCode',
  'welcome.quick.projectOverview': 'Project Overview',
  'welcome.quick.improvements': 'Find Improvements',
  'welcome.quick.devPlan': 'Create Plan',
  'welcome.quick.configuration': 'Configuration Help',
  'welcome.quick.tips': 'Usage Tips',

  'setup.account': 'Account',
  'setup.signedInAs': 'Signed in as {name}',
  'setup.atomgitUser': 'AtomGit user',
  'setup.signInHint': 'Sign in to use AtomGit CodingPlan models.',
  'setup.refreshAccount': 'Refresh account',
  'setup.signInWithAtomGit': 'Sign in with AtomGit',
  'setup.copy': 'Copy',
  'setup.cancel': 'Cancel',
  'setup.models': 'Models',
  'setup.providersConfigured': '{count} providers configured',
  'setup.syncOrAddProvider': 'Sync CodingPlan models or add a provider manually.',
  'setup.syncCodingPlanModels': 'Sync CodingPlan models',
  'setup.addProviderManually': 'Add provider manually',
  'setup.providerName': 'Provider name',
  'setup.providerType': 'Type, e.g. openai',
  'setup.model': 'Model',
  'setup.baseUrl': 'Base URL',
  'setup.apiKey': 'API key',
  'setup.saveProvider': 'Save provider',
  'setup.waitingForBrowser': 'Waiting for browser authorization...',
  'setup.signedInNextStep': 'Signed in. Sync CodingPlan models or add a provider.',

  'header.openSessions': 'Open sessions sidebar',
  'header.newConversation': 'New conversation',
  'header.search': 'Search',

  'input.imageTooLarge': 'Images must be under {mb} MB.',
  'input.insertPath': 'Insert path',
  'input.chooseFile': 'Choose file',
  'input.searchWorkspace': 'Search workspace',
  'input.uploadImage': 'Upload image',
  'input.searchProjectFiles': 'Search project files...',
  'input.noMatchingFiles': 'No matching files',
  'input.typeToSearchFiles': 'Type to search workspace files',
  'input.folder': 'Folder',
  'input.dismiss': 'Dismiss',
  'input.removeImage': 'Remove image',
  'input.placeholder': 'Type a message...',
  'input.commands': 'Commands',
  'input.attachFile': 'Attach file',
  'input.queueMessage': 'Queue message',
  'input.stop': 'Stop',
  'input.send': 'Send',

  'session.select': 'Select',
  'session.done': 'Done',
  'session.close': 'Close',
  'session.new': 'New session',
  'session.searchPlaceholder': 'Search sessions...',
  'session.selectedCount': '{count} selected',
  'session.deleteSelected': 'Delete selected',
  'session.cancelSelection': 'Cancel selection',
  'session.empty': 'No sessions yet',
  'session.untitled': 'Untitled',
  'session.rename': 'Rename',
  'session.delete': 'Delete session',
  'session.today': 'Today',
  'session.yesterday': 'Yesterday',
  'session.thisWeek': 'This Week',
  'session.older': 'Older',

  'search.placeholder': 'Search messages...',
  'search.found': '{count} found',
  'search.position': '{current}/{total}',
  'search.noResults': 'No results',
  'search.prev': 'Previous match',
  'search.next': 'Next match',
  'search.close': 'Close search',
  'search.scrollLatest': 'Scroll to latest',

  'model.effortTitle': 'Reasoning effort',
  'model.effortLabel': 'Effort',
  'model.effortDefault': 'Default',
  'model.selectModel': 'Select model',
  'model.noModels': 'No models available',
  'model.defaultBadge': 'default',
  'model.loginRequired': 'sign in required',
  'mode.title': 'Approval mode',
  'mode.build': 'Build',
  'mode.buildDesc': 'Ask before changes',
  'mode.acceptEdits': 'Accept Edits',
  'mode.acceptEditsDesc': 'Auto-approve edits, ask for commands',
  'mode.auto': 'Auto',
  'mode.autoDesc': 'Auto-approve all actions',
  'mode.plan': 'Plan',
  'mode.planDesc': 'Read-only planning',

  'slash.login': 'Sign in and sync CodingPlan models',
  'slash.logout': 'Sign out of AtomGit',
  'slash.whoami': 'Show current logged-in user',
  'slash.status': 'Show session status',
  'slash.config': 'Show config path',
  'slash.reload': 'Reload config from disk',
  'slash.skill': 'Skill',

  'tool.copied': 'Copied!',
  'tool.copy': 'Copy {label}',
  'tool.input': 'input',
  'tool.output': 'output',
  'tool.waiting': 'waiting',
  'tool.error': 'error',
  'tool.applied': 'applied',
  'tool.done': 'done',
  'tool.destructive': 'destructive',
  'tool.waitingApproval': 'waiting approval',
  'tool.incomplete': 'incomplete',

  'todo.title': 'Tasks',
  'todo.progress': '{completed}/{total} completed',
  'todo.pending': 'Pending',
  'todo.inProgress': 'In progress',
  'todo.completed': 'Completed',
  'todo.expand': 'Expand task list',
  'todo.collapse': 'Collapse task list',

  'stream.rateLimitedRetrying': 'Rate limited. Retrying in {seconds}s...',
  'stream.rateLimitedPaused': 'Rate limited. Waiting to resume...',
  'stream.idleNotice': 'Still waiting for the model...',
  'stream.incomplete': 'Turn incomplete ({reason}). Partial results were preserved; continue in a new turn.',

  'permission.deny': 'Deny',
  'permission.allow': 'Allow',
  'permission.alwaysAllow': 'Always Allow',
  'permission.allowPersist': 'Allow & Persist',
  'permission.denying': 'Denying...',
  'permission.allowing': 'Allowing...',
  'permission.alwaysAllowing': 'Always Allowing...',
  'permission.persisting': 'Persisting...',

  'assistant.artifact': 'artifact',
  'assistant.streaming': 'streaming',
  'assistant.copied': 'Copied',
  'assistant.copy': 'Copy',
  'user.queued': 'Queued',
  'user.selection': 'Selection',
  'user.file': 'File',
  'user.imageUnavailable': 'Image unavailable',
  'user.expand': 'Expand',
  'user.collapse': 'Collapse',

  'image.preview': 'Image preview',
  'image.previous': 'Previous image',
  'image.next': 'Next image',
  'image.closePreview': 'Close preview',

  'provider.settingsTitle': 'AtomCode Settings',
  'provider.notSignedIn': 'Not signed in',
  'provider.providers': 'Providers',
  'provider.noneConfigured': 'No providers configured.',
  'provider.keySet': 'key set',
  'provider.use': 'Use',
  'provider.thinkOn': 'Think on',
  'provider.thinkOff': 'Think off',
  'provider.delete': 'Delete',
  'provider.refresh': 'Refresh',
  'provider.addProvider': 'Add Provider',
  'provider.typePlaceholder': 'Type, e.g. openai',
  'provider.saveProvider': 'Save provider',

  'time.justNow': 'just now',
  'time.minutesAgo': '{count}m ago',
  'time.hoursAgo': '{count}h ago',
  'time.daysAgo': '{count}d ago',
  'time.older': 'older',
  'token.count': '{count} tokens',
  'token.countK': '{count}k tokens',
};

export const messages: Record<Lang, Record<MsgKey, string>> = { zh, en };

export function normalizeLocale(locale?: string): Lang {
  const normalized = (locale ?? '').toLowerCase();
  return normalized.startsWith('zh') ? 'zh' : 'en';
}

export function createTranslator(locale?: string) {
  const lang = normalizeLocale(locale);
  return (key: MsgKey, params?: TParams): string => {
    let text = messages[lang][key] ?? messages.en[key] ?? key;
    if (params) {
      for (const [name, value] of Object.entries(params)) {
        text = text.split(`{${name}}`).join(String(value));
      }
    }
    return text;
  };
}

function defaultLocale(): string | undefined {
  return typeof document === 'undefined' ? undefined : document.body.dataset.locale;
}

const I18nContext = createContext<{
  lang: Lang;
  t: ReturnType<typeof createTranslator>;
}>({
  lang: normalizeLocale(defaultLocale()),
  t: createTranslator(defaultLocale()),
});

export function I18nProvider({ locale, children }: { locale?: string; children: React.ReactNode }) {
  const lang = normalizeLocale(locale);
  const t = useMemo(() => createTranslator(lang), [lang]);

  useEffect(() => {
    document.documentElement.setAttribute('lang', lang === 'zh' ? 'zh-CN' : 'en');
  }, [lang]);

  return (
    <I18nContext.Provider value={{ lang, t }}>
      {children}
    </I18nContext.Provider>
  );
}

export function useI18n() {
  return useContext(I18nContext);
}

export function useT() {
  return useI18n().t;
}
