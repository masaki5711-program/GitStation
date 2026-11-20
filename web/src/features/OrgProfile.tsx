// The organisation the identity provider (the PFS Keycloak) says a person
// belongs to — a snapshot taken at sign-in (ADR-0021 amendment 2026-11-20),
// display only. The primary department is listed first and marked with the
// text 「主」 (never by colour alone). Everyone else sees only the primary
// department next to a name (`OrgCode`): its department and section names
// from the copy of the PFS org master (ADR-0038 §1), or the code when the
// copy does not resolve it.
import { useQuery } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';

import { call } from '../api/client';
import { useApi } from '../api/context';
import type { components } from '../api/schema.gen';
import styles from './OrgProfile.module.css';

export type OrgProfileBody = components['schemas']['OrgProfileBody'];
export type OrgPathBody = components['schemas']['OrgPathBody'];
type OrgNameBody = components['schemas']['OrgNameBody'];

/** A unit's name in the UI language (en, then ja; id, then en, then ja). */
export function orgName(n: OrgNameBody, lang: string): string {
  if (lang.startsWith('en')) return n.name_en ?? n.name_ja;
  if (lang.startsWith('id')) return n.name_id ?? n.name_en ?? n.name_ja;
  return n.name_ja;
}

/** The names from the department down (`withUnit`: down to the unit itself),
 * each once. */
export function orgPathNames(path: OrgPathBody, lang: string, withUnit = false): string[] {
  const chain = [path.department, path.section, withUnit ? path.unit : null];
  const seen = new Set<string>();
  const names: string[] = [];
  for (const n of chain) {
    if (n === null || seen.has(n.code)) continue;
    seen.add(n.code);
    names.push(orgName(n, lang));
  }
  return names.length > 0 ? names : [orgName(path.unit, lang)];
}

export const OWN_ORG_KEY = ['org-profile', 'own'] as const;

/** The caller's own snapshot (`null`: none yet). */
export function useOwnOrgProfile(enabled = true) {
  const api = useApi();
  return useQuery({
    queryKey: OWN_ORG_KEY,
    enabled,
    queryFn: ({ signal }) => call(() => api.GET('/api/v1/user/org-profile', { signal })),
    select: (r) => r.org,
  });
}

/** 「最終確認時点」: the names are the last known ones (ADR-0038 §4: the
 * person's snapshot or the copy of the org master is invalid or old). */
function LastKnown({ path }: { path: OrgPathBody }) {
  const { t } = useTranslation();
  if (!path.stale) return null;
  return <span className={styles.lastKnown}>{t('org.lastKnown')}</span>;
}

/** A person's primary department next to their name: the department and
 * section names when the org master resolves them, else the code. */
export function OrgCode({
  code,
  path,
}: {
  code: string | null | undefined;
  path?: OrgPathBody | null;
}) {
  const { t, i18n } = useTranslation();
  if (path !== null && path !== undefined) {
    return (
      <span className={styles.code} title={t('org.codeTitle', { code: path.unit.code })}>
        <span className={styles.srOnly}>{t('org.unit')} </span>
        {orgPathNames(path, i18n.language).join(' / ')}
        <LastKnown path={path} />
      </span>
    );
  }
  if (code === null || code === undefined || code === '') return null;
  return (
    <span className={styles.code} title={t('org.departmentCode')}>
      <span className={styles.srOnly}>{t('org.departmentCode')} </span>
      {code}
    </span>
  );
}

function Units({ org }: { org: OrgProfileBody }) {
  const { t, i18n } = useTranslation();
  return (
    <ul className={styles.units}>
      {org.org_units.map((u) => (
        <li key={u.code} className={styles.unit}>
          {u.primary && <span className={styles.primary}>{t('org.primary')}</span>}
          {u.path !== null && (
            <span className={styles.name}>
              {orgPathNames(u.path, i18n.language, true).join(' / ')}
              <LastKnown path={u.path} />
            </span>
          )}
          <code>{u.code}</code>
        </li>
      ))}
    </ul>
  );
}

/** The whole snapshot (the caller's own, or an administrator's view). */
export function OrgDetails({ org }: { org: OrgProfileBody | null }) {
  const { t } = useTranslation();
  if (org === null) return <p className={styles.note}>{t('org.none')}</p>;
  const plant = org.plant_code ?? org.plants[0] ?? null;
  return (
    <>
      {org.invalid_since !== null && (
        <p role="status" className={styles.warning}>
          {t('org.invalid')}
        </p>
      )}
      <dl className={styles.list}>
        <dt>{t('org.departments')}</dt>
        <dd>{org.org_units.length === 0 ? t('org.empty') : <Units org={org} />}</dd>
        <dt>{t('org.positions')}</dt>
        <dd>{org.positions.length === 0 ? t('org.empty') : org.positions.join(', ')}</dd>
        <dt>{t('org.plant')}</dt>
        <dd>{plant === null ? t('org.empty') : <code>{plant}</code>}</dd>
      </dl>
    </>
  );
}

/** The user menu's one line: the departments (primary first) and the plant. */
export function OrgLine({ org }: { org: OrgProfileBody | null | undefined }) {
  const { t } = useTranslation();
  if (org === null || org === undefined || org.org_units.length === 0) return null;
  return (
    <div className={styles.compact} aria-label={t('org.title')}>
      <Units org={org} />
      {org.plant_code !== null && (
        <span className={styles.note}>
          {t('org.plant')} <code>{org.plant_code}</code>
        </span>
      )}
    </div>
  );
}
