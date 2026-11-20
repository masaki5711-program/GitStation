// The organisation the identity provider (the PFS Keycloak) says a person
// belongs to — a snapshot taken at sign-in (ADR-0021 amendment 2026-11-20),
// display only. The primary department is listed first and marked with the
// text 「主」 (never by colour alone). Everyone else sees only the primary
// department code next to a name (`OrgCode`).
import { useQuery } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';

import { call } from '../api/client';
import { useApi } from '../api/context';
import type { components } from '../api/schema.gen';
import styles from './OrgProfile.module.css';

export type OrgProfileBody = components['schemas']['OrgProfileBody'];

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

/** A person's primary department code next to their name. */
export function OrgCode({ code }: { code: string | null | undefined }) {
  const { t } = useTranslation();
  if (code === null || code === undefined || code === '') return null;
  return (
    <span className={styles.code} title={t('org.departmentCode')}>
      <span className={styles.srOnly}>{t('org.departmentCode')} </span>
      {code}
    </span>
  );
}

function Units({ org }: { org: OrgProfileBody }) {
  const { t } = useTranslation();
  return (
    <ul className={styles.units}>
      {org.org_units.map((u) => (
        <li key={u.code} className={styles.unit}>
          {u.primary && <span className={styles.primary}>{t('org.primary')}</span>}
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
