const { spawnSync } = require('node:child_process');
const { mkdtempSync, rmSync } = require('node:fs');
const { tmpdir } = require('node:os');
const path = require('node:path');
const esbuild = require('esbuild');

async function main() {
  const root = path.join(__dirname, '..', '..');
  const tempDir = mkdtempSync(path.join(tmpdir(), 'atomcode-vscode-webview-tests-'));
  const tests = [
    'language-bridge.test.ts',
    'language-bridge-lifecycle.test.ts',
    'rendering-regression.test.ts',
    'file-position.test.ts',
    'i18n-regression.test.ts',
    'quick-actions-regression.test.ts',
    'contextual-prompt-regression.test.ts',
    'at-mention.test.ts',
    'slash-picker-regression.test.ts',
    'daemon-client-error.test.ts',
    'auth-status.test.ts',
    'daemon-process-env.test.ts',
    'daemon-identity.test.ts',
    'daemon-connection.test.ts',
    'provider-queue-regression.test.ts',
    'mode-state.test.ts',
    'mode-selector-style.test.ts',
    'session-list-regression.test.ts',
    'search.test.ts',
    'todo-regression.test.ts',
    'tab-title-truncation.test.ts',
    'daemon-client-token.test.ts',
    'daemon-client-token-retry.test.ts',
    'effort-levels.test.ts',
  ];

  // Keep going after the first failure: stopping there hides the real state of every file
  // after it (one CRLF-sensitive assertion used to mask 21 files on Windows). The summary
  // below still exits non-zero, so a red suite stays red.
  const failed = [];
  try {
    for (const test of tests) {
      const input = path.join(__dirname, test);
      const output = path.join(tempDir, test.replace(/\.ts$/, '.cjs'));
      try {
        await esbuild.build({
          entryPoints: [input],
          bundle: true,
          platform: 'node',
          format: 'cjs',
          target: 'node20',
          outfile: output,
          external: ['vscode'],
          logLevel: 'silent',
        });
      } catch (err) {
        failed.push({ test, status: 'build failed' });
        console.error(`\n[webview-tests] ${test} did not build: ${err?.message ?? err}`);
        continue;
      }

      const result = spawnSync(process.execPath, [output], {
        cwd: root,
        stdio: 'inherit',
      });
      if (result.status !== 0) {
        failed.push({ test, status: result.status ?? 'killed' });
      }
    }
  } finally {
    rmSync(tempDir, { recursive: true, force: true });
  }

  if (failed.length > 0) {
    console.error(`\n[webview-tests] ${failed.length} of ${tests.length} file(s) failed:`);
    for (const failure of failed) {
      console.error(`  - ${failure.test} (${failure.status})`);
    }
    process.exit(1);
  }
  console.log(`[webview-tests] all ${tests.length} file(s) passed`);
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
