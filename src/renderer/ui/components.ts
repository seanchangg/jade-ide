/* ═══════════════════════════════════════════
   Forge IDE — Component primitives
   Vanilla-DOM factories that follow the shadcn/ui recipes.
   They emit plain elements carrying stable class names (.ui-btn, .ui-badge,
   …) that main.css styles. No framework, no runtime deps.
   ═══════════════════════════════════════════ */

import { icon, IconName } from './icons';

export type ButtonVariant =
  | 'default'
  | 'secondary'
  | 'ghost'
  | 'outline'
  | 'destructive';

export type ButtonSize = 'default' | 'sm' | 'icon';

export interface ButtonOptions {
  label?: string;
  icon?: IconName;
  /** Icon shown after the label (e.g. a chevron). */
  trailingIcon?: IconName;
  variant?: ButtonVariant;
  size?: ButtonSize;
  title?: string;
  id?: string;
  /** Extra classes appended after the ui-btn classes. */
  className?: string;
  onClick?: (ev: MouseEvent) => void;
  disabled?: boolean;
}

function iconSizeFor(size: ButtonSize): number {
  return size === 'sm' ? 14 : 16;
}

/**
 * Build a shadcn-style button element.
 * Heights come from CSS: 32px default, 28px sm, 32px square icon.
 */
export function button(opts: ButtonOptions = {}): HTMLButtonElement {
  const el = document.createElement('button');
  el.type = 'button';
  const variant = opts.variant ?? 'default';
  const size = opts.size ?? 'default';
  el.className = [
    'ui-btn',
    `ui-btn-${variant}`,
    size !== 'default' ? `ui-btn-${size}` : '',
    opts.className ?? '',
  ]
    .filter(Boolean)
    .join(' ');

  if (opts.id) el.id = opts.id;
  if (opts.title) el.title = opts.title;
  if (opts.disabled) el.disabled = true;

  const iSize = iconSizeFor(size);
  if (opts.icon) {
    const span = document.createElement('span');
    span.className = 'ui-btn-icon';
    span.innerHTML = icon(opts.icon, iSize);
    el.appendChild(span);
  }
  if (opts.label) {
    const l = document.createElement('span');
    l.className = 'ui-btn-label';
    l.textContent = opts.label;
    el.appendChild(l);
  }
  if (opts.trailingIcon) {
    const span = document.createElement('span');
    span.className = 'ui-btn-icon';
    span.innerHTML = icon(opts.trailingIcon, iSize);
    el.appendChild(span);
  }
  if (opts.onClick) el.addEventListener('click', opts.onClick);
  return el;
}

/** Square, icon-only button (shadcn `size="icon"`). */
export function iconButton(
  opts: Omit<ButtonOptions, 'size' | 'label'> & { icon: IconName },
): HTMLButtonElement {
  return button({ variant: 'ghost', ...opts, size: 'icon' });
}

/** Imperatively update a button's text label (preserves its icon). */
export function setButtonLabel(el: HTMLElement | null, text: string): void {
  const l = el?.querySelector('.ui-btn-label');
  if (l) l.textContent = text;
}

export type BadgeVariant =
  | 'default'
  | 'secondary'
  | 'outline'
  | 'success'
  | 'warning'
  | 'destructive';

export interface BadgeOptions {
  label?: string;
  icon?: IconName;
  variant?: BadgeVariant;
  id?: string;
  title?: string;
  className?: string;
}

/**
 * Build a shadcn-style badge/pill. When `icon` is set the text is wrapped in a
 * `.ui-badge-text` span so it can be updated without clobbering the icon.
 */
export function badge(opts: BadgeOptions = {}): HTMLSpanElement {
  const el = document.createElement('span');
  const variant = opts.variant ?? 'default';
  el.className = ['ui-badge', `ui-badge-${variant}`, opts.className ?? '']
    .filter(Boolean)
    .join(' ');
  if (opts.id) el.id = opts.id;
  if (opts.title) el.title = opts.title;
  if (opts.icon) {
    const span = document.createElement('span');
    span.className = 'ui-badge-icon';
    span.innerHTML = icon(opts.icon, 13);
    el.appendChild(span);
  }
  if (opts.label !== undefined) {
    const t = document.createElement('span');
    t.className = 'ui-badge-text';
    t.textContent = opts.label;
    el.appendChild(t);
  }
  return el;
}

/** Update the text inside a badge built with `badge()`. */
export function setBadgeText(el: HTMLElement | null, text: string): void {
  const t = el?.querySelector('.ui-badge-text');
  if (t) t.textContent = text;
  else if (el) el.textContent = text;
}

export type TooltipSide = 'top' | 'bottom' | 'left' | 'right';

/**
 * Attach a CSS-only tooltip to `el`. Renders via a `::after` bubble driven by
 * the `data-tooltip` attribute (styled in main.css). Returns `el` for chaining.
 */
export function tooltip<T extends HTMLElement>(
  el: T,
  text: string,
  side: TooltipSide = 'bottom',
): T {
  el.setAttribute('data-tooltip', text);
  el.setAttribute('data-tooltip-side', side);
  el.classList.add('ui-has-tooltip');
  return el;
}

/** A 1px shadcn separator. `orientation` defaults to horizontal. */
export function separator(
  orientation: 'horizontal' | 'vertical' = 'horizontal',
): HTMLDivElement {
  const el = document.createElement('div');
  el.className = `ui-separator ui-separator-${orientation}`;
  el.setAttribute('role', 'separator');
  return el;
}
