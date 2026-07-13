// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

import { svelte } from "@sveltejs/vite-plugin-svelte";
import { defineConfig } from "vitest/config";

export default defineConfig({
  plugins: [svelte()],
  // Tauri convention: a fixed port for `build.devUrl`, and no clearing of the
  // terminal, which would hide Rust errors.
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
  },
  // Without this, vitest resolves svelte's SERVER build and `mount()` refuses
  // to run. Tests only: the app build is already "browser".
  resolve: process.env.VITEST ? { conditions: ["browser"] } : undefined,
  test: {
    environment: "happy-dom",
  },
});
