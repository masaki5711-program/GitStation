// A project's bindings to kanflow (M3 I1a / X3): which integration and which
// kanflow project this repository is bound to. Binding is done from kanflow,
// acting for the person with their confirmation (ADR-0025 Amendment X3), so
// this section only shows the state. A Maintainer (or an administrator) may
// unbind directly (`DELETE …/integration-bindings/{integration_id}`); the
// button is shown only to them, after a confirmation, and the server decides
// again.
import { useState } from 'react';
import { useTranslation } from 'react-i18next';

import { useProjectBindings, useUnbindProject, type IntegrationBinding } from '../../api/integrations';
import type { Project } from '../../api/types';
import { Loading } from '../../components/Loading';
import { QueryError } from '../../components/QueryError';
import { formatDateTime } from '../../features/labels';
import { useSession } from '../../session/session';
import { ConfirmDialog, Section, useConfirm, useFocusHeading, useRefreshSessionOn403 } from './common';
import styles from './Settings.module.css';

/** The roles the unbind route allows (`IntegrationBindingManage`: Maintainer). */
export function mayUnbind(role: Project['role'], isAdmin: boolean): boolean {
  return isAdmin || role === 'maintainer' || role === 'owner';
}

export function IntegrationBindingsSection({ project }: { project: Project }) {
  const { t, i18n } = useTranslation();
  const session = useSession();
  const focusHeading = useFocusHeading();
  const refresh = useRefreshSessionOn403();
  const bindings = useProjectBindings(project.id);
  const unbind = useUnbindProject(project.id);
  const confirm = useConfirm<IntegrationBinding>();
  const [notice, setNotice] = useState('');
  const canUnbind = mayUnbind(project.role, session.user.is_admin);
  const nameOf = (b: Pick<IntegrationBinding, 'kind' | 'integration_id'>) =>
    t('integrations.name', { kind: b.kind, id: b.integration_id });
  const target = confirm.target;
  return (
    <Section id="integration-bindings-title" title={t('integrations.projectTitle')}>
      <p className={styles.hint}>{t('integrations.projectExplain')}</p>
      <p role="status" className={styles.muted}>
        {notice}
      </p>
      {bindings.isPending && <Loading />}
      {bindings.isError && (
        <QueryError
          error={bindings.error}
          onRetry={() => {
            void bindings.refetch();
          }}
        />
      )}
      {bindings.isSuccess &&
        (bindings.data.items.length === 0 ? (
          <p>{t('integrations.projectNone')}</p>
        ) : (
          <div className={styles.scroll} role="region" aria-label={t('integrations.projectTitle')} tabIndex={0}>
            <table className={styles.table}>
              <thead>
                <tr>
                  <th scope="col">{t('integrations.integration')}</th>
                  <th scope="col">{t('integrations.kanflowProject')}</th>
                  <th scope="col">{t('integrations.membership')}</th>
                  <th scope="col">{t('integrations.boundAt')}</th>
                  {canUnbind && <th scope="col">{t('common.actions')}</th>}
                </tr>
              </thead>
              <tbody>
                {bindings.data.items.map((b) => (
                  <tr key={`${String(b.integration_id)}/${b.external_project_id}`}>
                    <th scope="row">{nameOf(b)}</th>
                    <td>
                      <code>{b.external_project_id}</code>
                    </td>
                    <td>
                      {b.granted_membership ? t('integrations.grantedMembership') : t('integrations.ownMembership')}
                    </td>
                    <td>{formatDateTime(b.created_at, i18n.language)}</td>
                    {canUnbind && (
                      <td>
                        <button
                          type="button"
                          className={`${styles.dangerQuiet} ${styles.touch}`}
                          aria-label={t('integrations.unbindOf', {
                            name: nameOf(b),
                            project: b.external_project_id,
                          })}
                          onClick={() => {
                            unbind.reset();
                            confirm.ask(b);
                          }}
                        >
                          {t('integrations.unbind')}
                        </button>
                      </td>
                    )}
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        ))}
      <ConfirmDialog
        open={target !== null}
        title={t('integrations.unbindTitle')}
        message={
          <>
            <p>
              {t('integrations.unbindBody', {
                name: target === null ? '' : nameOf(target),
                project: target?.external_project_id ?? '',
              })}
            </p>
            {target?.granted_membership === true && <p>{t('integrations.unbindMembership')}</p>}
            <p>{t('integrations.unbindAgain')}</p>
          </>
        }
        confirmLabel={t('integrations.unbind')}
        busy={unbind.isPending}
        error={unbind.isError ? unbind.error : null}
        onConfirm={() => {
          if (target === null) return;
          unbind.mutate(target.integration_id, {
            onSuccess: () => {
              confirm.close();
              setNotice(t('integrations.unbound', { name: nameOf(target) }));
              focusHeading('integration-bindings-title');
            },
            onError: refresh,
          });
        }}
        onCancel={confirm.close}
      />
    </Section>
  );
}
