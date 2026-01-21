import { defineConfig, loadEnv } from 'vite'
import react from '@vitejs/plugin-react'

export default defineConfig(({ mode }) => {
  const env = loadEnv(mode, process.cwd(), '')
  
  const apiTarget = env.VITE_API_TARGET || 'http://127.0.0.1:9090'
  const serverPort = parseInt(env.VITE_PORT || '3333', 10)

  return {
    plugins: [react()],
    server: {
      port: serverPort,
      proxy: {
        '/controller': {
          target: apiTarget,
          changeOrigin: true,
          rewrite: (path) => path.replace(/^\/controller/, ''),
        },
      },
    },
  }
})
