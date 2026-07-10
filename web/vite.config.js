import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

export default defineConfig({
  plugins: [react()],
  base: './',
  build: {
    outDir: 'dist',
    // One JS chunk, one CSS file — simplest possible embed.
    rollupOptions: { output: { manualChunks: undefined } },
  },
  server: {
    proxy: { '/v1': 'http://127.0.0.1:7717' },
  },
})
