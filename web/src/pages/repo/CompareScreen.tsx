// Two revisions compared: the commits `to` has and `from` does not (the
// server lists 200 at most: `commits_truncated`), and the tree diff from
// `from` to `to` (capped; `truncated`). The address is
// `/-/compare/{from}...{to}`: `...` cannot be in a ref name.
import { useId, useState, type SyntheticEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { useNavigate } from 'react-router';

import { useCompare, useRefs } from '../../api/repo';
import { Loading } from '../../components/Loading';
import controls from '../../components/controls.module.css';
import { DiffView } from '../../features/DiffView';
import { compareUrl } from '../../features/repoUrls';
import { useDocumentTitle } from '../../hooks/useDocumentTitle';
import { CommitList } from './CommitsScreen';
import { RepoError } from './common';
import styles from './Repo.module.css';

/** `a...b` from the address tail; `null` when it is not of that form. */
export function parseCompare(spec: string): { from: string; to: string } | null {
  const i = spec.indexOf('...');
  if (i <= 0) return null;
  const from = spec.slice(0, i);
  const to = spec.slice(i + 3);
  return from === '' || to === '' ? null : { from, to };
}

function CompareForm({
  projectId,
  project,
  parsed,
}: {
  projectId: number;
  project: string;
  parsed: { from: string; to: string } | null;
}) {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const refs = useRefs(projectId);
  const [from, setFrom] = useState(parsed?.from ?? '');
  const [to, setTo] = useState(parsed?.to ?? refs.data?.default_branch ?? '');
  const fromId = useId();
  const toId = useId();
  const listId = useId();

  const submit = (e: SyntheticEvent) => {
    e.preventDefault();
    if (from.trim() === '' || to.trim() === '') return;
    void navigate(compareUrl(project, from.trim(), to.trim()));
  };

  return (
    <form onSubmit={submit} className={styles.compareForm}>
      <div className={styles.field}>
        <label htmlFor={fromId}>{t('compare.from')}</label>
        <input
          id={fromId}
          className={controls.input}
          list={listId}
          value={from}
          onChange={(e) => {
            setFrom(e.target.value);
          }}
          autoComplete="off"
        />
      </div>
      <div className={styles.field}>
        <label htmlFor={toId}>{t('compare.to')}</label>
        <input
          id={toId}
          className={controls.input}
          list={listId}
          value={to}
          onChange={(e) => {
            setTo(e.target.value);
          }}
          autoComplete="off"
        />
      </div>
      <datalist id={listId}>
        {(refs.data?.refs ?? []).map((r) => (
          <option key={r.full_name} value={r.name} />
        ))}
      </datalist>
      <button type="submit" className={controls.primary}>
        {t('compare.submit')}
      </button>
    </form>
  );
}

export function CompareScreen({
  projectId,
  project,
  spec,
}: {
  projectId: number;
  project: string;
  spec: string;
}) {
  const { t } = useTranslation();
  const parsed = parseCompare(spec);
  const compare = useCompare(projectId, parsed?.from ?? null, parsed?.to ?? null);
  useDocumentTitle(t('compare.title'));

  return (
    <>
      <h1 className={styles.title}>{t('compare.title')}</h1>
      {/* Keyed by the address: the inputs follow it (back / forward, a link). */}
      <CompareForm key={spec} projectId={projectId} project={project} parsed={parsed} />
      <p className={styles.hint}>{t('compare.hint')}</p>

      {parsed === null && <p>{t('compare.choose')}</p>}
      {parsed !== null && compare.isPending && <Loading />}
      {compare.isError && (
        <RepoError
          error={compare.error}
          onRetry={() => {
            void compare.refetch();
          }}
        />
      )}
      {compare.isSuccess && (
        <>
          <h2 className={styles.sectionTitle}>
            {t('compare.commits', { count: compare.data.commits.length })}
          </h2>
          {compare.data.commits_truncated && (
            <p className={styles.warning} role="note" data-testid="compare-truncated">
              {t('compare.commitsTruncated', { count: compare.data.commits.length })}
            </p>
          )}
          {compare.data.commits.length === 0 ? (
            <p>{t('compare.noCommits')}</p>
          ) : (
            <CommitList project={project} commits={compare.data.commits} />
          )}
          <h2 className={styles.sectionTitle}>{t('commit.changes')}</h2>
          <DiffView diff={compare.data.diff} project={project} refName={compare.data.to.name} />
        </>
      )}
    </>
  );
}
