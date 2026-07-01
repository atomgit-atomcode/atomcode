type QuickActionPromptMap = Record<string, string>;

const enPrompts: QuickActionPromptMap = {
  intro: 'Introduce what AtomCode can help me do.',
  projectOverview: 'Summarize the structure and main modules of the current workspace.',
  improvements: 'Analyze the current project and identify code quality, test, or documentation improvements.',
  devPlan: 'Create a practical next-step development plan for the current project.',
  configuration: 'Help me check the AtomCode, model, and Provider configuration, and explain how to configure them.',
  tips: 'Introduce common workflows and tips for using AtomCode in VS Code.',
  explain: 'Please explain this code. What does it do and why?',
  fix: 'Please fix any bugs or issues in this code.',
  test: 'Please generate unit tests for this code.',
  refactor: 'Please refactor this code for better readability and maintainability.',
  docs: 'Please add documentation comments to this code.',
  review: 'Please review this code for issues, improvements, and best practices.',
  optimize: 'Please optimize this code for performance while preserving behavior.',
};

const zhPrompts: QuickActionPromptMap = {
  intro: '介绍一下 AtomCode 可以帮我做什么。',
  projectOverview: '请根据当前工作区，帮我总结这个项目的结构和主要模块。',
  improvements: '请分析当前项目，找出可以改进的代码质量、测试或文档问题。',
  devPlan: '请根据当前项目，帮我制定一个下一步开发计划。',
  configuration: '请帮我检查 AtomCode、模型和 Provider 配置，并说明如何配置。',
  tips: '请介绍在 VS Code 中使用 AtomCode 的常用工作流和技巧。',
  explain: '请解释这段代码。它做了什么，为什么这样做？',
  fix: '请修复这段代码中的 bug 或问题。',
  test: '请为这段代码生成单元测试。',
  refactor: '请重构这段代码，以提高可读性和可维护性。',
  docs: '请为这段代码添加文档注释。',
  review: '请审查这段代码中的问题、改进点和最佳实践。',
  optimize: '请在保持行为不变的前提下优化这段代码的性能。',
};

function isChineseLocale(locale?: string) {
  return locale?.toLowerCase().startsWith('zh') ?? false;
}

export function getQuickActionPrompt(action: string, locale?: string): string {
  const prompts = isChineseLocale(locale) ? zhPrompts : enPrompts;
  return prompts[action] ?? action;
}
