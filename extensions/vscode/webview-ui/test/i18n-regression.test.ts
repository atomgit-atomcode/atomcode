import assert from 'node:assert/strict';
import { existsSync, readFileSync } from 'node:fs';
import { join } from 'node:path';
import { createTranslator, messages, normalizeLocale } from '../src/i18n';

function testLocaleNormalizationFollowsVSCodeLanguage() {
  assert.equal(normalizeLocale('zh-cn'), 'zh');
  assert.equal(normalizeLocale('zh-CN'), 'zh');
  assert.equal(normalizeLocale('zh-tw'), 'zh');
  assert.equal(normalizeLocale('en-US'), 'en');
  assert.equal(normalizeLocale('fr'), 'en');
  assert.equal(normalizeLocale(undefined), 'en');
}

function testTranslatorFallsBackToEnglishAndInterpolatesValues() {
  const zh = createTranslator('zh-CN');
  const en = createTranslator('en-US');

  assert.equal(zh('welcome.quick.intro'), '了解 AtomCode');
  assert.equal(en('welcome.quick.intro'), 'Learn AtomCode');
  assert.equal(zh('setup.providersConfigured', { count: 3 }), '已配置 3 个 Provider');
  assert.equal(zh('mode.auto'), 'Auto');
  assert.equal(en('mode.auto'), 'Auto');
}

function testCatalogsHaveMatchingKeys() {
  const zhKeys = Object.keys(messages.zh).sort();
  const enKeys = Object.keys(messages.en).sort();
  assert.deepEqual(zhKeys, enKeys);
}

function testVSCodeManifestUsesNlsPlaceholders() {
  const root = process.cwd();
  const packageJson = readFileSync(join(root, 'package.json'), 'utf8');

  assert.match(packageJson, /"l10n":\s*"\.\/l10n"/);
  assert.match(packageJson, /"title":\s*"%atomcode\.commands\.openSidebar\.title%"/);
  assert.match(packageJson, /"shortTitle":\s*"%atomcode\.commands\.explain\.shortTitle%"/);
  assert.match(packageJson, /"shortTitle":\s*"%atomcode\.commands\.fix\.shortTitle%"/);
  assert.match(packageJson, /"shortTitle":\s*"%atomcode\.commands\.optimize\.shortTitle%"/);
  assert.match(packageJson, /"shortTitle":\s*"%atomcode\.commands\.addToChat\.shortTitle%"/);
  assert.match(packageJson, /"description":\s*"%atomcode\.configuration\.daemon\.port\.description%"/);
  assert.ok(existsSync(join(root, 'package.nls.json')));
  assert.ok(existsSync(join(root, 'package.nls.zh-cn.json')));
}

function testPackageNlsFilesCoverEveryManifestPlaceholder() {
  const root = process.cwd();
  const packageJson = readFileSync(join(root, 'package.json'), 'utf8');
  const en = JSON.parse(readFileSync(join(root, 'package.nls.json'), 'utf8')) as Record<string, string>;
  const zhCn = JSON.parse(readFileSync(join(root, 'package.nls.zh-cn.json'), 'utf8')) as Record<string, string>;
  const placeholders = Array.from(packageJson.matchAll(/%([^%]+)%/g), (match) => match[1]).sort();

  assert.ok(placeholders.length > 0);
  for (const key of placeholders) {
    assert.ok(en[key], `English package.nls.json missing ${key}`);
    assert.ok(zhCn[key], `Chinese package.nls.zh-cn.json missing ${key}`);
  }
}

testLocaleNormalizationFollowsVSCodeLanguage();
testTranslatorFallsBackToEnglishAndInterpolatesValues();
testCatalogsHaveMatchingKeys();
testVSCodeManifestUsesNlsPlaceholders();
testPackageNlsFilesCoverEveryManifestPlaceholder();
