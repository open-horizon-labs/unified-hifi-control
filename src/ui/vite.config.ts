import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import path from 'path'

// https://vite.dev/config/
export default defineConfig({
  plugins: [react()],
  resolve: {
    alias: {
      '@': path.resolve(__dirname, './src'),
    },
  },
  build: {
    outDir: '../../dist/ui',
    emptyOutDir: true,
  },
  server: {
    proxy: {
      '/api': 'http://localhost:3000',
      '/admin': 'http://localhost:3000',
      '/control': 'http://localhost:3000',
      '/roon': 'http://localhost:3000',
      '/hqp': 'http://localhost:3000',
      '/lms': 'http://localhost:3000',
      '/config': 'http://localhost:3000',
      '/now_playing': 'http://localhost:3000',
      '/firmware': 'http://localhost:3000',
    },
  },
})
