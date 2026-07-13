// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

import { mount } from "svelte";

import "./app.css";
import App from "./App.svelte";
import { CoreStore } from "./lib/store.svelte";

// Outside Tauri (`npm run dev` in a browser), we wire up a fake Core: the
// screens are visible without a daemon or webview. This branch disappears from
// the production bundle, since `import.meta.env.DEV` is `false` there.
if (import.meta.env.DEV && !("__TAURI_INTERNALS__" in window)) {
  const { installFakeCore } = await import("./dev/fake-core");
  installFakeCore();
}

const app = mount(App, {
  target: document.getElementById("app")!,
  props: { store: new CoreStore() },
});

export default app;
