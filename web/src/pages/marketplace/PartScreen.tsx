// /-/marketplace/{slug} (plan 16 A8, §6): one agent part — every published
// version newest first with its channel, its capability marks in words and
// its state; the newest live version's files and signature facts. "Make
// stable" and "Revoke" are shown only to whom the server would allow them
// (a Maintainer of the publishing group or an administrator; promoting a
// part that uses the network: an administrator), decided from the group's
// role the API exposes. The server decides again.
import { useState, type SyntheticEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { Link, useParams } from 'react-router';

import {
  mayManageParts,
  mayPromote,
  mayRevoke,
  usePart,
  usePartVersion,
  usePromote,
  usePublishingGroupRole,
  useRevoke,
  type PartVersion,
} from '../../api/marketplace';
import { TextField } from '../../components/Field';
import controls from '../../components/controls.module.css';
import { QueryError } from '../../components/QueryError';
import { SkeletonList } from '../../components/Skeleton';
import { ErrorSummary } from '../../features/CreateDialogs';
import { formatBytes, formatDateTime } from '../../features/labels';
import { CapabilityMarks, ChannelBadge, RevokedBadge, kindLabel } from '../../features/marketplace/marks';
import { useDocumentTitle } from '../../hooks/useDocumentTitle';
import { useSession } from '../../session/session';
import { Modal } from '../../ui/Modal';
import { PageHeader } from '../../ui/PageHeader';
import { Denied } from '../settings/common';
import settings from '../settings/Settings.module.css';
import styles from './Marketplace.module.css';
import { MARKETPLACE_PATH } from './MarketplaceScreen';

/** `/-/marketplace/:slug`: the part the address names. */
export function PartRoute() {
  const { slug } = useParams();
  const s = slug ?? '';
  return <PartScreen key={s} slug={s} />;
}

export function PartScreen({ slug }: { slug: string }) {
  const { t, i18n } = useTranslation();
  const session = useSession();
  const isAdmin = session.user.is_admin;
  const part = usePart(slug);
  useDocumentTitle(`${slug} · ${t('market.title')}`);
  const versions = part.data?.versions ?? [];
  const live = versions.find((v) => v.state === 'published') ?? null;
  const { role } = usePublishingGroupRole(versions[0]?.source_project_id ?? null);
  const detail = usePartVersion(slug, live?.version ?? null);
  const promote = usePromote();
  const [promoted, setPromoted] = useState('');
  const [revoking, setRevoking] = useState<string | null>(null);

  const back = (
    <Link to={MARKETPLACE_PATH} className={styles.back}>
      {t('market.back')}
    </Link>
  );
  if (part.isPending) {
    return (
      <div className={styles.page}>
        {back}
        <SkeletonList rows={3} />
      </div>
    );
  }
  if (part.isError) {
    return (
      <div className={styles.page}>
        {back}
        <QueryError
          error={part.error}
          onRetry={() => {
            void part.refetch();
          }}
        />
      </div>
    );
  }
  const kind = versions[0]?.kind ?? '';
  const manage = mayManageParts(role, isAdmin);
  return (
    <div className={styles.page}>
      {back}
      <PageHeader title={slug} subtitle={kindLabel(t, kind)}>
        <div className={styles.headMarks}>
          {live === null ? <RevokedBadge /> : <ChannelBadge channel={live.channel} />}
          {live !== null && <CapabilityMarks caps={live.capabilities} />}
        </div>
      </PageHeader>
      {live === null && <p className={styles.warning}>{t('market.part.allRevoked')}</p>}

      <section className={styles.section} aria-labelledby="part-versions">
        <h2 id="part-versions" className={styles.sectionTitle}>
          {t('market.part.versions')}
        </h2>
        <div className={settings.scroll} role="region" aria-label={t('market.part.versions')} tabIndex={0}>
          <table className={settings.table}>
            <caption className={settings.srOnly}>{t('market.part.versions')}</caption>
            <thead>
              <tr>
                <th scope="col">{t('market.part.version')}</th>
                <th scope="col">{t('market.part.state')}</th>
                <th scope="col">{t('market.capabilities')}</th>
                <th scope="col">{t('market.part.published')}</th>
                {manage && <th scope="col">{t('common.actions')}</th>}
              </tr>
            </thead>
            <tbody>
              {versions.map((v) => (
                <VersionRow
                  key={v.version}
                  v={v}
                  manage={manage}
                  canPromote={mayPromote(v, role, isAdmin)}
                  canRevoke={mayRevoke(v, role, isAdmin)}
                  busy={promote.isPending}
                  onPromote={() => {
                    setPromoted('');
                    promote.mutate(
                      { slug, version: v.version },
                      {
                        onSuccess: () => {
                          setPromoted(t('market.part.promoted', { version: v.version }));
                        },
                      },
                    );
                  }}
                  onRevoke={() => {
                    setRevoking(v.version);
                  }}
                  language={i18n.language}
                />
              ))}
            </tbody>
          </table>
        </div>
        <p role="status" className={styles.statusLine}>
          {promoted}
        </p>
        {promote.isError && <Denied error={promote.error} />}
      </section>

      {live !== null && (
        <section className={styles.section} aria-labelledby="part-files">
          <h2 id="part-files" className={styles.sectionTitle}>
            {t('market.part.files', { version: live.version })}
          </h2>
          <dl className={styles.facts}>
            <dt>{t('market.part.tag')}</dt>
            <dd>
              <code>{live.tag}</code>
            </dd>
            <dt>{t('market.part.digest')}</dt>
            <dd>
              <code className={styles.hash}>{live.digest}</code>
            </dd>
            <dt>{t('market.part.key')}</dt>
            <dd>
              <code>{live.key_id}</code>
            </dd>
          </dl>
          {detail.isPending && <SkeletonList rows={2} />}
          {detail.isError && (
            <QueryError
              error={detail.error}
              onRetry={() => {
                void detail.refetch();
              }}
            />
          )}
          {detail.isSuccess && (
            <ul className={styles.files}>
              {detail.data.files.map((f) => (
                <li key={f.path}>
                  <code>{f.path}</code>
                  <span className={styles.muted}>
                    {f.mode === '100755'
                      ? t('market.part.fileExec', { size: formatBytes(f.size, i18n.language) })
                      : formatBytes(f.size, i18n.language)}
                  </span>
                </li>
              ))}
            </ul>
          )}
          <p className={styles.hint}>{t('market.part.verifyHint')}</p>
        </section>
      )}
      <RevokeDialog
        slug={slug}
        version={revoking}
        onClose={() => {
          setRevoking(null);
        }}
      />
    </div>
  );
}

function VersionRow({
  v,
  manage,
  canPromote,
  canRevoke,
  busy,
  onPromote,
  onRevoke,
  language,
}: {
  v: PartVersion;
  manage: boolean;
  canPromote: boolean;
  canRevoke: boolean;
  busy: boolean;
  onPromote: () => void;
  onRevoke: () => void;
  language: string;
}) {
  const { t } = useTranslation();
  const revoked = v.state !== 'published';
  const networkNote = manage && !canPromote && !revoked && v.channel === 'beta' && v.capabilities.includes('network');
  return (
    <tr className={revoked ? settings.rowMuted : undefined}>
      <th scope="row">
        <code>{v.version}</code>
      </th>
      <td>
        {revoked ? (
          <>
            <RevokedBadge />
            {v.revoked_reason !== null && <span className={styles.reason}>{v.revoked_reason}</span>}
          </>
        ) : (
          <ChannelBadge channel={v.channel} />
        )}
      </td>
      <td>
        <CapabilityMarks caps={v.capabilities} />
      </td>
      <td>{formatDateTime(v.created_at, language)}</td>
      {manage && (
        <td>
          <span className={settings.inline}>
            {canPromote && (
              <button
                type="button"
                className={controls.button}
                aria-label={t('market.part.promoteOf', { version: v.version })}
                aria-disabled={busy ? 'true' : undefined}
                onClick={() => {
                  if (!busy) onPromote();
                }}
              >
                {t('market.part.promote')}
              </button>
            )}
            {canRevoke && (
              <button
                type="button"
                className={settings.dangerQuiet}
                aria-label={t('market.part.revokeOf', { version: v.version })}
                onClick={onRevoke}
              >
                {t('market.part.revoke')}
              </button>
            )}
          </span>
          {networkNote && <p className={styles.note}>{t('market.part.networkAdminOnly')}</p>}
        </td>
      )}
    </tr>
  );
}

function RevokeDialog({ slug, version, onClose }: { slug: string; version: string | null; onClose: () => void }) {
  const { t } = useTranslation();
  const revoke = useRevoke();
  return (
    <Modal
      open={version !== null}
      busy={revoke.isPending}
      title={t('market.part.revokeOf', { version: version ?? '' })}
      onClose={() => {
        revoke.reset();
        onClose();
      }}
    >
      {version !== null && (
        <RevokeForm
          revoke={revoke}
          onSubmit={(reason) => {
            revoke.mutate({ slug, version, reason }, { onSuccess: onClose });
          }}
        />
      )}
    </Modal>
  );
}

function RevokeForm({ revoke, onSubmit }: { revoke: ReturnType<typeof useRevoke>; onSubmit: (reason: string) => void }) {
  const { t } = useTranslation();
  const [reason, setReason] = useState('');
  const [attempt, setAttempt] = useState(0);
  const r = reason.trim();
  const error =
    r === '' ? t('market.part.reasonEmpty') : Array.from(r).length > 500 ? t('market.part.reasonLong') : null;
  const submit = (e: SyntheticEvent) => {
    e.preventDefault();
    if (revoke.isPending) return;
    setAttempt((n) => n + 1);
    if (error !== null) return;
    onSubmit(r);
  };
  return (
    <form onSubmit={submit} noValidate className={styles.dialogForm}>
      <p>{t('market.part.revokeExplain')}</p>
      <ErrorSummary errors={error === null ? [] : [error]} attempt={attempt} />
      <TextField
        label={t('market.part.reason')}
        value={reason}
        onChange={setReason}
        error={attempt > 0 ? error : null}
        multiline
        required
        initialFocus
      />
      {revoke.isError && <Denied error={revoke.error} />}
      <div className={styles.actions}>
        <button type="submit" className={settings.danger} aria-disabled={revoke.isPending ? 'true' : undefined}>
          {t('market.part.revoke')}
        </button>
      </div>
    </form>
  );
}
