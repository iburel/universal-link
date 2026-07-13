// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

/** Mounting Svelte components for tests (no test dependency). */

import { flushSync, mount, unmount } from "svelte";

import type { CoreStore } from "./store.svelte";

/** The `.svelte` files don't expose a component type usable here. */
type AnyComponent = Parameters<typeof mount>[0];
type Instance = Record<string, unknown>;

const mounted: Instance[] = [];

export function render(component: AnyComponent, props: object): HTMLElement {
  const target = document.createElement("div");
  document.body.appendChild(target);
  mounted.push(mount(component, { target, props }) as Instance);
  flushSync();
  return target;
}

export function cleanup(): void {
  for (const instance of mounted.splice(0)) void unmount(instance);
  document.body.innerHTML = "";
}

/** Clicks and lets reactivity propagate. */
export function click(element: Element | null | undefined): void {
  if (!element) throw new Error("missing element");
  (element as HTMLElement).click();
  flushSync();
}

export function type(input: Element | null | undefined, value: string): void {
  if (!input) throw new Error("missing input");
  (input as HTMLInputElement).value = value;
  input.dispatchEvent(new Event("input", { bubbles: true }));
  flushSync();
}

export function press(element: Element | null | undefined, key: string): void {
  if (!element) throw new Error("missing element");
  element.dispatchEvent(new KeyboardEvent("keydown", { key, bubbles: true }));
  flushSync();
}

export function byLabel(root: ParentNode, label: string): HTMLElement {
  const element = root.querySelector(`[aria-label="${label}"]`);
  if (!element) throw new Error(`no element labeled "${label}"`);
  return element as HTMLElement;
}

/** The first element whose text contains `text`. */
export function byText(
  root: ParentNode,
  selector: string,
  text: string,
): HTMLElement {
  for (const element of root.querySelectorAll(selector)) {
    if (element.textContent?.includes(text)) return element as HTMLElement;
  }
  throw new Error(`no ${selector} containing "${text}"`);
}

export function textOf(root: ParentNode): string {
  return (root as HTMLElement).textContent ?? "";
}

/** A store in a given state, without going through the IPC. */
export function storeWith(
  base: CoreStore,
  state: Partial<CoreStore>,
): CoreStore {
  Object.assign(base, state);
  return base;
}
