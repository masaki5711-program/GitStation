// `/{project}/-/webhooks/{id}/deliveries` (U3a): one webhook's delivery
// history, newest first - state, attempts, response code, time - and a
// redeliver button. A refusal (429 while a delivery is still in progress or
// within a minute of its last attempt; 400 for a stopped webhook) is shown as
// the server worded it, with its request id. Bodies are never stored, so none
// is shown.
import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Link } from 'react-router';

import { useDeliveries, useRedeliver, useWebhook, type Delivery } from '../../api/webhooks';
import { Loading } from '../../components/Loading';
import { QueryError } from '../../components/QueryError';
import controls from '../../components/controls.module.css';
import { formatDateTime } from '../../features/labels';
import { encodePath } from '../../features/repoUrls';
import { useDocumentTitle } from '../../hooks/useDocumentTitle';
import { Denied } from '../settings/common';
import styles from '../settings/Settings.module.css';
import { EnabledChip, StopReason } from './common';
import { PageHeader } from '../../ui/PageHeader';

function stateChip(state: string): string {
  switch (state) {
    case 'succeeded':
      return styles.chipActive ?? '';
    case 'failed':
      return styles.chipRevoked ?? '';
    default:
      return styles.chipExpired ?? '';
  }
}

export function DeliveriesScreen({
  projectId,
  project,
  hookId,
}: {
  projectId: number;
  project: string;
  hookId: number;
}) {
  const { t } = useTranslation();
  const hook = useWebhook(projectId, hookId);
  const deliveries = useDeliveries(projectId, hookId);
  const redeliver = useRedeliver(projectId, hookId);
  const [message, setMessage] = useState('');
  useDocumentTitle(t('webhooks.historyTitle'));
  const back = `/${encodePath(project)}/-/webhooks`;
  const items = deliveries.data?.pages.flatMap((p) => p.items) ?? [];

  return (
    <>
      <p>
        <Link to={back}>{t('webhooks.back')}</Link>
      </p>
      <PageHeader title={t('webhooks.historyTitle')} />
      {hook.isError && (
        <QueryError
          error={hook.error}
          onRetry={() => {
            void hook.refetch();
          }}
        />
      )}
      {hook.isSuccess && (
        <>
          <p>
            <code>{hook.data.origin}</code> <EnabledChip enabled={hook.data.enabled} />
          </p>
          <StopReason hook={hook.data} />
        </>
      )}
      <p role="status" className={styles.muted}>
        {message}
      </p>
      {redeliver.isError && <Denied error={redeliver.error} />}
      {deliveries.isPending && <Loading />}
      {deliveries.isError && (
        <QueryError
          error={deliveries.error}
          onRetry={() => {
            void deliveries.refetch();
          }}
        />
      )}
      {deliveries.isSuccess &&
        (items.length === 0 ? (
          <p>{t('webhooks.noDeliveries')}</p>
        ) : (
          <>
            <DeliveryTable
              items={items}
              busy={redeliver.isPending}
              onRedeliver={(d) => {
                setMessage('');
                redeliver.mutate(d.id, {
                  onSuccess: () => {
                    setMessage(t('webhooks.queued'));
                  },
                });
              }}
            />
            {deliveries.hasNextPage && (
              <div className={styles.row}>
                <button
                  type="button"
                  className={controls.button}
                  aria-disabled={deliveries.isFetchingNextPage ? 'true' : undefined}
                  onClick={() => {
                    if (deliveries.isFetchingNextPage) return;
                    // Clear first, so the same count twice is announced again.
                    setMessage('');
                    const before = items.length;
                    void deliveries.fetchNextPage().then((r) => {
                      const now = r.data?.pages.flatMap((p) => p.items).length ?? before;
                      if (now > before) setMessage(t('webhooks.olderLoaded', { count: now - before }));
                    });
                  }}
                >
                  {deliveries.isFetchingNextPage ? t('common.loading') : t('webhooks.older')}
                </button>
              </div>
            )}
          </>
        ))}
    </>
  );
}

function DeliveryTable({
  items,
  busy,
  onRedeliver,
}: {
  items: Delivery[];
  busy: boolean;
  onRedeliver: (d: Delivery) => void;
}) {
  const { t, i18n } = useTranslation();
  const stateLabel = (state: string) => {
    switch (state) {
      case 'succeeded':
        return t('webhooks.deliveryState.succeeded');
      case 'failed':
        return t('webhooks.deliveryState.failed');
      case 'skipped':
        return t('webhooks.deliveryState.skipped');
      default:
        return t('webhooks.deliveryState.pending');
    }
  };
  return (
    <div className={styles.scroll} role="region" aria-label={t('webhooks.historyTitle')} tabIndex={0}>
      <table className={styles.table}>
        <thead>
          <tr>
            <th scope="col">{t('webhooks.time')}</th>
            <th scope="col">{t('webhooks.event')}</th>
            <th scope="col">{t('webhooks.deliveryStateLabel')}</th>
            <th scope="col">{t('webhooks.attempts')}</th>
            <th scope="col">{t('webhooks.response')}</th>
            <th scope="col">{t('common.actions')}</th>
          </tr>
        </thead>
        <tbody>
          {items.map((d) => (
            <tr key={d.id}>
              <td>
                <time dateTime={d.created_at}>{formatDateTime(d.created_at, i18n.language)}</time>
              </td>
              <td>
                <code>{d.event_type}</code>
              </td>
              <td>
                <span className={`${styles.chip} ${stateChip(d.state)}`}>{stateLabel(d.state)}</span>
              </td>
              <td>{d.attempts}</td>
              <td>
                {d.last_status !== null ? (
                  d.last_status
                ) : d.last_reason !== null ? (
                  <code>{d.last_reason}</code>
                ) : (
                  t('webhooks.noResponse')
                )}
              </td>
              <td>
                <button
                  type="button"
                  className={`${controls.button} ${styles.touch}`}
                  aria-label={t('webhooks.redeliverOf', { event: d.event_type, id: d.id })}
                  aria-disabled={busy ? 'true' : undefined}
                  onClick={() => {
                    if (!busy) onRedeliver(d);
                  }}
                >
                  {t('webhooks.redeliver')}
                </button>
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
