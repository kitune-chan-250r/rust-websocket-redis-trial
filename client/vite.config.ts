import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// https://vite.dev/config/
export default defineConfig({
  plugins: [react()],
  server: {
    host: true,
    port: 5173,
    watch: {
      usePolling: true,
      interval: 100,
    },
    // プロキシの設定
    proxy: {
      // '/api' で始まるパスへのリクエストをプロキシする
      "/ws": {
        ws: true,
        // 転送先
        target: "ws://rust:8000/ws",
        // オリジンを転送先に変更
        changeOrigin: true,
        // パスの書き換え（例: /api/users -> /users）
        // rewrite: (path) => path.replace(/^\/api/, ""),
      },
    },
    // devcontainer内で外部からアクセスするためにhost設定が必要な場合があります
    // host: "0.0.0.0",
  },
});
