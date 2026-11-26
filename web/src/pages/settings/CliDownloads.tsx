// The gstn CLI distribution: the binaries this server ships (only the targets
// actually present), each with its size and SHA-256, and the steps to set it
// up. The download is a plain same-origin link: the session cookie
// authenticates it and the server answers with Content-Disposition.
import { useQuery } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';

import { call } from '../../api/client';
import { useApi } from '../../api/context';
import type { components } from '../../api/schema.gen';
import { Loading } from '../../components/Loading';
import { QueryError } from '../../components/QueryError';
import { formatBytes } from '../../features/labels';
import { Section } from './common';
import styles from './Settings.module.css';

type Client = components['schemas']['ClientItem'];

// Commands, not prose: the same in every language.
const SETUP_COMMANDS = ['gstn --version', 'gstn auth login --with-token < token.txt', 'gstn auth status'].join(
  '\n',
);

function targetLabel(t: (key: string) => string, target: string): string {
  if (target === 'windows-x86_64') return t('cli.windows');
  if (target === 'linux-x86_64') return t('cli.linux');
  return target;
}

export function CliDownloads() {
  const { t } = useTranslation();
  const api = useApi();
  const clients = useQuery({
    queryKey: ['clients'],
    queryFn: ({ signal }) => call(() => api.GET('/api/v1/clients', { signal })),
  });
  return (
    <Section id="cli-title" title={t('cli.title')}>
      <p className={styles.hint}>
        {t('cli.explain')} <a href="#cli-setup">{t('cli.setupLink')}</a>
      </p>
      {clients.isPending && <Loading />}
      {clients.isError && (
        <QueryError
          error={clients.error}
          onRetry={() => {
            void clients.refetch();
          }}
        />
      )}
      {clients.isSuccess &&
        (clients.data.items.length === 0 ? (
          <p className={styles.hint}>{t('cli.none')}</p>
        ) : (
          <ClientTable clients={clients.data.items} />
        ))}
      <h3 id="cli-setup" className={styles.subTitle} tabIndex={-1}>
        {t('cli.setupTitle')}
      </h3>
      <ol className={styles.steps}>
        <li>{t('cli.stepPlace')}</li>
        <li>{t('cli.stepVerify')}</li>
        <li>{t('cli.stepLogin')}</li>
      </ol>
      <pre className={styles.pre}>
        <code>{SETUP_COMMANDS}</code>
      </pre>
    </Section>
  );
}

function ClientTable({ clients }: { clients: Client[] }) {
  const { t, i18n } = useTranslation();
  return (
    <div className={styles.scroll}>
      <table className={styles.table}>
        <thead>
          <tr>
            <th scope="col">{t('cli.target')}</th>
            <th scope="col">{t('cli.version')}</th>
            <th scope="col">{t('cli.size')}</th>
            <th scope="col">{t('cli.sha256')}</th>
            <th scope="col">{t('common.actions')}</th>
          </tr>
        </thead>
        <tbody>
          {clients.map((c) => {
            const label = targetLabel(t, c.target);
            return (
              <tr key={c.target}>
                <th scope="row">{label}</th>
                <td>{c.version}</td>
                <td>{formatBytes(c.size, i18n.language)}</td>
                <td>
                  <code className={styles.hash}>{c.sha256}</code>
                </td>
                <td>
                  <a href={c.download_path} download data-testid={`cli-download-${c.target}`}>
                    {t('cli.download', { target: label })}
                  </a>
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}
