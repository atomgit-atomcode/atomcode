const esbuild = require('esbuild');
const path = require('path');

const watch = process.argv.includes('--watch');
const minify = process.env.NODE_ENV === 'production';

const config = {
  entryPoints: [path.join(__dirname, 'src/index.tsx')],
  bundle: true,
  outdir: path.join(__dirname, '..', 'webview'),
  entryNames: 'webview',
  format: 'iife',
  platform: 'browser',
  target: 'es2020',
  loader: { '.tsx': 'tsx', '.ts': 'ts', '.css': 'css' },
  minify,
  sourcemap: true,
  define: { 'process.env.NODE_ENV': minify ? '"production"' : '"development"' },
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
