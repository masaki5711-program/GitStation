// /-/marketplace (plan 16 A8, §6): the agent parts catalog — tabs by kind
// (`?kind=`), the channel (`?channel=beta|stable`) and a slug search
// (`?q=`), each part with its newest live version, its channel and its
// capability marks in words. "Publish a part" is shown only to a Maintainer
// of the publishing group or an administrator (decided from the group's
// role the API exposes; the server decides again).
import { useState, type SyntheticEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { Link, useSearchParams } from 'react-router';

import { PART_KINDS, mayManageParts, parseKind, useParts, usePublishingGroupRole, type PartSummary } from '../../api/marketplace';
import controls from '../../components/controls.module.css';
import { QueryError } from '../../components/QueryError';
import { SkeletonList } from '../../components/Skeleton';
import { formatDateTime } from '../../features/labels';
import { CapabilityMarks, ChannelBadge, kindLabel } from '../../features/marketplace/marks';
import { useDocumentTitle } from '../../hooks/useDocumentTitle';
import { useSession } from '../../session/session';
import { EmptyState } from '../../ui/EmptyState';
import { PageHeader } from '../../ui/PageHeader';
import tabStyles from '../settings/Settings.module.css';
import styles from './Marketplace.module.css';
import { PublishDialog } from './PublishDialog';

export const MARKETPLACE_PATH = '/-/marketplace';

export const partUrl = (slug: string) => `${MARKETPLACE_PATH}/${encodeURIComponent(slug)}`;

function listUrl(kind: string | null, channel: string | null, q: string): string {
  const p = new URLSearchParams();
  if (kind !== null) p.set('kind', kind);
  if (channel !== null) p.set('channel', channel);
  if (q !== '') p.set('q', q);
  const s = p.toString();
  return s === '' ? MARKETPLACE_PATH : `${MARKETPLACE_PATH}?${s}`;
}

function parseChannel(value: string | null): 'beta' | 'stable' | null {
  return value === 'beta' || value === 'stable' ? value : null;
}

export function MarketplaceScreen() {
  const { t } = useTranslation();
  const session = useSession();
  const [params, setParams] = useSearchParams();
  const kind = parseKind(params.get('kind'));
  const channel = parseChannel(params.get('channel'));
  const q = (params.get('q') ?? '').trim().slice(0, 64);
  const [draft, setDraft] = useState(q);
  const list = useParts(kind, channel, q);
  // Every part comes from the one parts project: any part names it.
  const all = useParts(null, null, '');
  const source = all.data?.parts.find((p) => p.latest !== null)?.latest?.source_project_id ?? null;
  const { role } = usePublishingGroupRole(source);
  const canPublish = mayManageParts(role, session.user.is_admin);
  const [publishing, setPublishing] = useState(false);
  useDocumentTitle(t('market.title'));

  const tabs: { id: string; label: string; to: string; current: boolean }[] = [
    { id: 'all', label: t('market.kind.all'), to: listUrl(null, channel, q), current: kind === null },
    ...PART_KINDS.map((k) => ({ id: k, label: kindLabel(t, k), to: listUrl(k, channel, q), current: kind === k })),
  ];
  const search = (e: SyntheticEvent) => {
    e.preventDefault();
    const next = new URLSearchParams(params);
    const v = draft.trim().slice(0, 64);
    if (v === '') next.delete('q');
    else next.set('q', v);
    setParams(next);
  };

  return (
    <div className={styles.page}>
      <PageHeader
        title={t('market.title')}
        subtitle={t('market.subtitle')}
        actions={
          canPublish ? (
            <button
              type="button"
              className={controls.primary}
              onClick={() => {
                setPublishing(true);
              }}
            >
              {t('market.publish.open')}
            </button>
          ) : undefined
        }
      />
      <nav aria-label={t('market.kinds')}>
        <ul className={tabStyles.tabs}>
          {tabs.map((x) => (
            <li key={x.id}>
              <Link to={x.to} className={tabStyles.tab} {...(x.current ? { 'aria-current': 'page' as const } : {})}>
                {x.label}
              </Link>
            </li>
          ))}
        </ul>
      </nav>
      <div className={styles.filters}>
        <div className={controls.field}>
          <label htmlFor="market-channel" className={controls.label}>
            {t('market.channel')}
          </label>
          <select
            id="market-channel"
            className={controls.input}
            value={channel ?? ''}
            onChange={(e) => {
              const next = new URLSearchParams(params);
              const v = parseChannel(e.target.value);
              if (v === null) next.delete('channel');
              else next.set('channel', v);
              setParams(next);
            }}
          >
            <option value="">{t('market.channelAll')}</option>
            <option value="stable">{t('market.channelStable')}</option>
            <option value="beta">{t('market.channelBeta')}</option>
          </select>
        </div>
        <form role="search" className={styles.search} onSubmit={search}>
          <div className={controls.field}>
            <label htmlFor="market-q" className={controls.label}>
              {t('market.search')}
            </label>
            <input
              id="market-q"
              className={controls.input}
              type="search"
              value={draft}
              maxLength={64}
              onChange={(e) => {
                setDraft(e.target.value);
              }}
            />
          </div>
          <button type="submit" className={controls.button}>
            {t('market.searchButton')}
          </button>
        </form>
      </div>
      {list.isPending && <SkeletonList rows={4} />}
      {list.isError && (
        <QueryError
          error={list.error}
          onRetry={() => {
            void list.refetch();
          }}
        />
      )}
      {list.isSuccess &&
        (list.data.parts.length === 0 ? (
          <EmptyState
            icon="search"
            reason={t('market.empty')}
            hints={[t('market.emptyHint')]}
            live
            action={
              kind !== null || channel !== null || q !== '' ? (
                <Link to={MARKETPLACE_PATH} className={controls.button}>
                  {t('market.clearFilters')}
                </Link>
              ) : undefined
            }
          />
        ) : (
          <ul className={styles.grid} aria-label={t('market.listLabel')}>
            {list.data.parts.map((p) => (
              <PartCard key={p.slug} part={p} />
            ))}
          </ul>
        ))}
      <PublishDialog
        open={publishing}
        parts={all.data?.parts ?? []}
        onClose={() => {
          setPublishing(false);
        }}
      />
    </div>
  );
}

function PartCard({ part }: { part: PartSummary }) {
  const { t, i18n } = useTranslation();
  const v = part.latest;
  return (
    <li className={styles.card}>
      <div className={styles.cardHead}>
        <h2 className={styles.cardTitle}>
          <Link to={partUrl(part.slug)} className={styles.cardLink}>
            {part.slug}
          </Link>
        </h2>
        {v !== null && <ChannelBadge channel={v.channel} />}
      </div>
      <p className={styles.meta}>
        {v === null
          ? t('market.noLive', { kind: kindLabel(t, part.kind) })
          : t('market.cardMeta', {
              kind: kindLabel(t, part.kind),
              version: v.version,
              at: formatDateTime(v.created_at, i18n.language),
            })}
      </p>
      {part.deprecated && <p className={styles.deprecated}>{t('market.deprecated')}</p>}
      {v !== null && <CapabilityMarks caps={v.capabilities} />}
    </li>
  );
}
