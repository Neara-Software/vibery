import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

export default defineConfig({
  plugins: [react()],
  server: { port: 5173, proxy: { "/api": process.env.VIBERY_API ?? "http://127.0.0.1:7878" } },
  build: { outDir: "dist", target: "es2022" },
});
