import assert from 'node:assert/strict';
import { getQuickActionPrompt } from '../../src/chat/quickActions';

function testQuickActionPromptsFollowChineseLocale() {
  assert.equal(getQuickActionPrompt('intro', 'zh-cn'), '介绍一下 AtomCode 可以帮我做什么。');
  assert.equal(getQuickActionPrompt('projectOverview', 'zh-CN'), '请根据当前工作区，帮我总结这个项目的结构和主要模块。');
  assert.equal(getQuickActionPrompt('improvements', 'zh'), '请分析当前项目，找出可以改进的代码质量、测试或文档问题。');
  assert.equal(getQuickActionPrompt('devPlan', 'zh-cn'), '请根据当前项目，帮我制定一个下一步开发计划。');
  assert.equal(getQuickActionPrompt('configuration', 'zh-cn'), '请帮我检查 AtomCode、模型和 Provider 配置，并说明如何配置。');
  assert.equal(getQuickActionPrompt('tips', 'zh-cn'), '请介绍在 VS Code 中使用 AtomCode 的常用工作流和技巧。');
}

function testQuickActionPromptsKeepEnglishFallback() {
  assert.equal(getQuickActionPrompt('intro', 'en-US'), 'Introduce what AtomCode can help me do.');
  assert.equal(getQuickActionPrompt('projectOverview', 'fr'), 'Summarize the structure and main modules of the current workspace.');
  assert.equal(getQuickActionPrompt('unknown-action', 'zh-cn'), 'unknown-action');
}

testQuickActionPromptsFollowChineseLocale();
testQuickActionPromptsKeepEnglishFallback();
