// Words for API enums. Literal keys only (the dictionary gate reads them).
import type { TFunction } from 'i18next';

export function visibilityLabel(t: TFunction, v: string): string {
  return v === 'internal' ? t('visibility.internal') : t('visibility.private');
}

export function kindLabel(t: TFunction, kind: string): string {
  return kind === 'spec' ? t('kind.spec') : t('kind.system');
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

/** `2026-10-01T00:00:00Z` as a date in the browser's time zone. */
export function formatDate(iso: string, language: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  return new Intl.DateTimeFormat(language, { dateStyle: 'medium' }).format(d);
}
