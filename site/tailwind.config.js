/** @type {import('tailwindcss').Config} */
module.exports = {
  content: ['./index.html', './docs/**/*.html'],
  darkMode: 'class',
  theme: {
    extend: {
      colors: {
        brand: { 50: '#f3f0ff', 100: '#e9e3ff', 200: '#d4cbff', 400: '#a78bfa', 500: '#8b5cf6', 600: '#7c3aed', 700: '#6d28d9', 800: '#5b21b6', 900: '#1e1033' },
        surface: { 50: '#f8fafc', 100: '#f1f5f9', 800: '#1e293b', 900: '#0f172a', 950: '#020617' }
      },
      fontFamily: {
        sans: ['-apple-system', 'BlinkMacSystemFont', '"PingFang SC"', '"Microsoft YaHei"', '"Segoe UI"', 'system-ui', 'sans-serif'],
        mono: ['"SF Mono"', 'SFMono-Regular', 'Menlo', 'Monaco', 'Consolas', '"Liberation Mono"', '"Courier New"', 'monospace']
      }
    }
  },
}
