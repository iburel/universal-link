// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

import { readFileSync } from "node:fs";

import { svelte } from "@sveltejs/vite-plugin-svelte";
import { defineConfig } from "vitest/config";

// The version the interface displays (see src/lib/version.ts). Read from
// package.json rather than handed in by whoever runs the build, so the bundle is
// the same wherever it is built — CI, the release runner, a developer's machine
// — and there is one file to bump. A test in the Rust shell pins that file to
// the crate's own version.
const { version } = JSON.parse(
  readFileSync(new URL("package.json", import.meta.url), "utf8"),
) as { version: string };

export default defineConfig({
  plugins: [svelte()],
  define: { __APP_VERSION__: JSON.stringify(version) },
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
