// Words for API enums. Literal keys only (the dictionary gate reads them).
import type { TFunction } from 'i18next';

import type { BadgeTone } from './mr/StateBadge';

export function visibilityLabel(t: TFunction, v: string): string {
  return v === 'internal' ? t('visibility.internal') : t('visibility.private');
}

export function roleLabel(t: TFunction, role: string | null): string | null {
  switch (role) {
    case 'guest':
      return t('role.guest');
    case 'reporter':
      return t('role.reporter');
    case 'developer':
      return t('role.developer');
    case 'maintainer':
      return t('role.maintainer');
    case 'owner':
      return t('role.owner');
    default:
      return null;
  }
}

/** A merge request's state (`open` / `merged` / `closed`; the list filter adds `all`). */
export function mrStateLabel(t: TFunction, state: string): string {
  switch (state) {
    case 'merged':
      return t('mr.state.merged');
    case 'closed':
      return t('mr.state.closed');
    case 'all':
      return t('mr.state.all');
    default:
      return t('mr.state.open');
  }
}

/** `1536` → `1.5 KiB` (binary units, as git and the servers count). */
export function formatBytes(bytes: number, language: string): string {
  const units = ['B', 'KiB', 'MiB', 'GiB'];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  const number = new Intl.NumberFormat(language, {
    maximumFractionDigits: unit === 0 ? 0 : 1,
  }).format(value);
  return `${number} ${units[unit] ?? 'B'}`;
}

/** Date and time in the browser's time zone (a commit's moment). */
export function formatDateTime(iso: string, language: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  return new Intl.DateTimeFormat(language, { dateStyle: 'medium', timeStyle: 'short' }).format(d);
}

/** Epoch seconds (a blame commit's time). */
export function formatEpoch(seconds: number, language: string): string {
  return formatDate(new Date(seconds * 1000).toISOString(), language);
}

/** `2026-10-01T00:00:00Z` as a date in the browser's time zone. */
export function formatDate(iso: string, language: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  return new Intl.DateTimeFormat(language, { dateStyle: 'medium' }).format(d);
}

/** A merge method (`merge_commit` / `squash` / `fast_forward`); an unknown
 * name is shown as it is. */
export function mergeMethodLabel(t: TFunction, method: string): string {
  switch (method) {
    case 'merge_commit':
      return t('mr.merge.method.mergeCommit');
    case 'squash':
      return t('mr.merge.method.squash');
    case 'fast_forward':
      return t('mr.merge.method.fastForward');
    default:
      return method;
  }
}

/** Help for each merge method (the dialog and the settings). */
export function mergeMethodHelp(t: TFunction, method: string): string {
  switch (method) {
    case 'merge_commit':
      return t('mr.merge.method.mergeCommitHelp');
    case 'squash':
      return t('mr.merge.method.squashHelp');
    case 'fast_forward':
      return t('mr.merge.method.fastForwardHelp');
    default:
      return '';
  }
}

/** A required check's run as the shared state badge: no report is never a
 * success (docs/api/merge-status.md), a run still going is neutral, and only
 * `completed` / `success` is green. */
export function checkBadge(
  t: TFunction,
  status: string | null,
  conclusion: string | null,
): { tone: BadgeTone; label: string } {
  if (status === null) return { tone: 'draft', label: t('mr.check.none') };
  if (status !== 'completed') return { tone: 'draft', label: t('mr.check.running') };
  if (conclusion === 'success') return { tone: 'open', label: t('mr.check.success') };
  return { tone: 'unresolved', label: t('mr.check.failed', { conclusion: conclusion ?? '-' }) };
}

/** `2026-11-08T…` → "3 days ago" in the page's language (Intl, no
 * dictionary). The moment itself goes in a `<time>` title. */
export function formatRelative(iso: string, language: string, now: number = Date.now()): string {
  const then = new Date(iso).getTime();
  if (Number.isNaN(then)) return iso;
  const seconds = Math.round((then - now) / 1000);
  const steps: [Intl.RelativeTimeFormatUnit, number][] = [
    ['year', 365 * 24 * 3600],
    ['month', 30 * 24 * 3600],
    ['week', 7 * 24 * 3600],
    ['day', 24 * 3600],
    ['hour', 3600],
    ['minute', 60],
  ];
  const format = new Intl.RelativeTimeFormat(language, { numeric: 'auto' });
  for (const [unit, size] of steps) {
    if (Math.abs(seconds) >= size) return format.format(Math.round(seconds / size), unit);
  }
  return format.format(seconds, 'second');
}
