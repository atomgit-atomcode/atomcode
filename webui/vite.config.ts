import { defineConfig } from 'vite';
import preact from '@preact/preset-vite';

export default defineConfig({
  plugins: [preact()],
  base: './',
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    // The oldest engines this page still has to be usable on — Huawei's
    // browser is Chromium 99, and older 360/QQ builds are around there. The
    // CSS minifier drops a declaration it thinks a later one overrides, so
    // without this it removed every `100vh` before `100dvh` and every fallback
    // before a `color-mix()` — exactly what those engines need.
    cssTarget: ['chrome99', 'edge99', 'safari15', 'firefox100'],
    // And the script, for the same engines: the bundle runs there today, and
    // this keeps newer syntax from quietly turning them into a blank page.
    target: ['chrome99', 'edge99', 'safari15', 'firefox100'],
  },
  server: { port: 5173 },
});
