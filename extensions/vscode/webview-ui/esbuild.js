const esbuild = require('esbuild');
const fs = require('fs');
const path = require('path');

const watch = process.argv.includes('--watch');
const minify = process.env.NODE_ENV === 'production';
const outdir = path.join(__dirname, '..', 'webview');

const stripTrailingWhitespacePlugin = {
  name: 'strip-trailing-whitespace',
  setup(build) {
    build.onEnd(() => {
      for (const file of ['webview.js', 'webview.css']) {
        const outputPath = path.join(outdir, file);
        if (!fs.existsSync(outputPath)) {
          continue;
        }
        const content = fs.readFileSync(outputPath, 'utf8');
        fs.writeFileSync(outputPath, content.replace(/[ \t]+$/gm, ''));
      }
    });
  },
};

const config = {
  entryPoints: [path.join(__dirname, 'src/index.tsx')],
  bundle: true,
  outdir,
  entryNames: 'webview',
  format: 'iife',
  platform: 'browser',
  target: 'es2020',
  loader: { '.tsx': 'tsx', '.ts': 'ts', '.css': 'css' },
  minify,
  sourcemap: true,
  define: { 'process.env.NODE_ENV': minify ? '"production"' : '"development"' },
  plugins: [stripTrailingWhitespacePlugin],
};

if (watch) {
  esbuild.context(config).then(ctx => {
    ctx.watch();
    console.log('[webview-ui] watching...');
  });
} else {
  esbuild.build(config).then(() => {
    console.log('[webview-ui] built successfully');
  });
}
