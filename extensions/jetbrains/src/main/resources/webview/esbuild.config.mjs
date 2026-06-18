import * as esbuild from 'esbuild'
import { fileURLToPath } from 'url'
import { dirname, join } from 'path'

const __dirname = dirname(fileURLToPath(import.meta.url))
const watch = process.argv.includes('--watch')

const ctx = await esbuild.context({
  entryPoints: [join(__dirname, 'src/index.tsx')],
  bundle: true,
  minify: true,
  sourcemap: false,
  target: ['es2020'],
  outfile: join(__dirname, 'bundle.js'),
  loader: { '.ts': 'tsx', '.tsx': 'tsx' },
})

if (watch) {
  await ctx.watch()
  console.log('Watching...')
} else {
  await ctx.rebuild()
  await ctx.dispose()
  console.log('Build complete: bundle.js')
}
