const { spawnSync } = require('node:child_process');
const { mkdtempSync, rmSync } = require('node:fs');
const { tmpdir } = require('node:os');
const path = require('node:path');
const esbuild = require('esbuild');

async function main() {
  const root = path.join(__dirname, '..', '..');
  const tempDir = mkdtempSync(path.join(tmpdir(), 'atomcode-vscode-webview-tests-'));
  const tests = [
    'rendering-regression.test.ts',
    'i18n-regression.test.ts',
    'quick-actions-regression.test.ts',
    'contextual-prompt-regression.test.ts',
    'at-mention.test.ts',
    'slash-picker-regression.test.ts',
    'daemon-client-error.test.ts',
    'auth-status.test.ts',
    'daemon-process-env.test.ts',
    'provider-queue-regression.test.ts',
    'mode-state.test.ts',
    'mode-selector-style.test.ts',
    'session-list-regression.test.ts',
    'search.test.ts',
    'todo-regression.test.ts',
  ];

  try {
    for (const test of tests) {
      const input = path.join(__dirname, test);
      const output = path.join(tempDir, test.replace(/\.ts$/, '.cjs'));
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

      const result = spawnSync(process.execPath, [output], {
        cwd: root,
        stdio: 'inherit',
      });
      if (result.status !== 0) {
        process.exit(result.status ?? 1);
      }
    }
  } finally {
    rmSync(tempDir, { recursive: true, force: true });
  }
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
