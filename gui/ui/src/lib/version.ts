// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

/**
 * The version of THIS interface — the one bundled into the app you are looking
 * at, substituted into the bundle at build time (`vite.config.ts`).
 *
 * Not the Core's, which the local API does not report: the two are built and
 * shipped together, but on Linux the Core runs from a copy refreshed when the
 * app starts, so between an upgrade and the next launch of the app an autostarted
 * Core is still the previous one.
 */
export const appVersion: string = __APP_VERSION__;
