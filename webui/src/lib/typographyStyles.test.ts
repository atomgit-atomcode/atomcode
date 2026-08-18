import { test } from 'node:test';
import assert from 'node:assert';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';

const root = process.cwd();
const appCss = readFileSync(join(root, 'src/styles/app.css'), 'utf8');
const themeCss = readFileSync(join(root, 'src/styles/theme.css'), 'utf8');

test('mixed CJK code text uses explicit fallbacks before a portable monospace fallback', () => {
  const stack = themeCss.match(/--app-monospace-font-family:([^;]+);/)?.[1] ?? '';

  assert.match(stack, /Consolas/);
  assert.match(stack, /'PingFang SC'/);
  assert.match(stack, /'Microsoft YaHei'/);
  assert.match(stack, /'Microsoft YaHei',monospace\s*$/);
});

test('conversation text and composer share the compact 16px type rung', () => {
  assert.match(appCss, /\.assistant-message-content\s*\{[^}]*font-size:\s*1rem;[^}]*line-height:\s*1\.75;/s);
  assert.match(appCss, /\.user-message-bubble\s*\{[^}]*font-size:\s*1rem;[^}]*line-height:\s*1\.5;/s);
  assert.match(appCss, /\.message-input\s*\{[^}]*font-size:\s*1rem;[^}]*line-height:\s*1\.5;/s);
});

test('display serif stays on branding while approval decisions use sans', () => {
  assert.match(appCss, /\.sidebar-brand-name\s*\{[^}]*font-family:\s*var\(--app-serif-font-family\);/s);
  assert.match(appCss, /\.landing-brand-name\s*\{[^}]*font-family:\s*var\(--app-serif-font-family\);/s);
  assert.match(appCss, /\.permission-title\s*\{[^}]*font-family:\s*var\(--app-sans-font-family\);/s);
  assert.match(appCss, /\.permission-lead\s*\{[^}]*font-family:\s*var\(--app-sans-font-family\);/s);
});

test('theme exposes semantic metadata and surface color rungs', () => {
  for (const token of [
    '--app-tertiary-foreground',
    '--app-caption-foreground',
    '--app-layer-1-background',
    '--app-layer-2-background',
    '--app-border-subtle',
    '--app-border-strong',
  ]) {
    assert.equal(themeCss.split(`${token}:`).length - 1, 3, `${token} must cover dark, light, and system-light themes`);
  }
});

function relativeLuminance(hex: string): number {
  const channels = hex.match(/../g)?.map((channel) => Number.parseInt(channel, 16) / 255) ?? [];
  const linear = channels.map((channel) => (
    channel <= 0.04045 ? channel / 12.92 : ((channel + 0.055) / 1.055) ** 2.4
  ));
  return 0.2126 * linear[0] + 0.7152 * linear[1] + 0.0722 * linear[2];
}

function contrastRatio(foreground: string, background: string): number {
  const foregroundLuminance = relativeLuminance(foreground);
  const backgroundLuminance = relativeLuminance(background);
  const lighter = Math.max(foregroundLuminance, backgroundLuminance);
  const darker = Math.min(foregroundLuminance, backgroundLuminance);
  return (lighter + 0.05) / (darker + 0.05);
}

test('metadata text rungs remain readable in dark and light themes', () => {
  const values = (token: string) => [
    ...themeCss.matchAll(new RegExp(`${token}:#([0-9a-f]{6})`, 'gi')),
  ].map((match) => match[1]);
  const backgrounds = values('--app-primary-background');

  for (const token of ['--app-tertiary-foreground', '--app-caption-foreground']) {
    const foregrounds = values(token);
    assert.equal(foregrounds.length, backgrounds.length, `${token} must be defined for every theme`);

    for (const [index, foreground] of foregrounds.entries()) {
      assert.ok(
        contrastRatio(foreground, backgrounds[index]) >= 4.5,
        `${token} theme ${index + 1} must meet WCAG AA contrast for small text`,
      );
    }
  }
});
