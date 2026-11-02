// Words for API enums. Literal keys only (the dictionary gate reads them).
import type { TFunction } from 'i18next';

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
