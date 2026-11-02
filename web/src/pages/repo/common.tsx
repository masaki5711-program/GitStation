// Pieces the repository screens share: the resolved `{ref}[/{path}]` of an
// address, the error display (an unresolved ref or path is a 404 with its own
// message), the ref switcher and the path crumbs.
import { useId, useState, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { Link, useNavigate } from 'react-router';

import { ApiFailure } from '../../api/client';
import { useRefs, useSpec, type Spec } from '../../api/repo';
import type { RefEntry, Rev } from '../../api/types';
import { ErrorNotice } from '../../components/ErrorNotice';
import { Loading } from '../../components/Loading';
import { QueryError } from '../../components/QueryError';
import controls from '../../components/controls.module.css';
import { treeUrl } from '../../features/repoUrls';
import styles from './Repo.module.css';

/** A failed read in a repository screen. 404 means the ref or path does not
 * exist (the project itself is visible, or we would not be here): say so with
 * the server's own message; anything else is the common error display. */
export function RepoError({ error, onRetry }: { error: unknown; onRetry?: () => void }) {
  const { t } = useTranslation();
  if (error instanceof ApiFailure && error.failure.kind === 'api' && error.failure.status === 404) {
    return <ErrorNotice failure={error.failure} title={t('repo.notFoundTitle')} />;
  }
  return <QueryError error={error} {...(onRetry ? { onRetry } : {})} />;
}

export function EmptyRepository({ project }: { project: string }) {
  const { t } = useTranslation();
  return (
    <section aria-labelledby="repo-empty-title">
      <h1 id="repo-empty-title">{t('project.emptyTitle')}</h1>
      <p>{t('repo.emptyBody')}</p>
      <p>
        <Link to={`/${project.split('/').map(encodeURIComponent).join('/')}`}>
          {t('repo.backToProject')}
        </Link>
      </p>
    </section>
  );
}

/** Resolves the address tail; renders `children` with the revision and path. */
export function WithSpec({
  projectId,
  project,
  spec,
  children,
}: {
  projectId: number;
  project: string;
  spec: string;
  children: (spec: Spec) => ReactNode;
}) {
  const resolved = useSpec(projectId, spec);
  if (resolved.empty) return <EmptyRepository project={project} />;
  if (resolved.status === 'pending') return <Loading />;
  if (resolved.status === 'error') {
    return (
      <RepoError
        error={resolved.error}
        onRetry={() => {
          void resolved.refetch();
        }}
      />
    );
  }
  if (resolved.data === undefined) return <EmptyRepository project={project} />;
  return <>{children(resolved.data)}</>;
}

/** A select over the branches and tags and a button: changing the select
 * only picks (arrow keys must not navigate, WCAG 3.2.2); the button goes to
 * `to(name)`. The form is keyed by the current ref, so it follows the address. */
export function RefSwitcher(props: {
  projectId: number;
  current: string;
  to: (ref: string) => string;
}) {
  return <SwitcherForm key={props.current} {...props} />;
}

function SwitcherForm({
  projectId,
  current,
  to,
}: {
  projectId: number;
  current: string;
  to: (ref: string) => string;
}) {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const id = useId();
  const [choice, setChoice] = useState(current);
  const refs = useRefs(projectId);
  const list = refs.data?.refs ?? [];
  const branches = list.filter((r) => r.type === 'branch');
  const tags = list.filter((r) => r.type === 'tag');
  const known = list.some((r) => r.name === current);
  return (
    <form
      className={styles.switcher}
      onSubmit={(e) => {
        e.preventDefault();
        if (choice !== current) void navigate(to(choice));
      }}
    >
      <label htmlFor={id}>{t('repo.switchRef')}</label>
      <select
        id={id}
        className={controls.input}
        value={choice}
        onChange={(e) => {
          setChoice(e.target.value);
        }}
      >
        {!known && <option value={current}>{current.slice(0, 12)}</option>}
        {branches.length > 0 && (
          <optgroup label={t('repo.branches')}>
            {branches.map((r) => (
              <option key={r.full_name} value={r.name}>
                {r.name}
              </option>
            ))}
          </optgroup>
        )}
        {tags.length > 0 && (
          <optgroup label={t('repo.tags')}>
            {tags.map((r) => (
              <option key={r.full_name} value={r.name}>
                {r.name}
              </option>
            ))}
          </optgroup>
        )}
      </select>
      <button type="submit" className={controls.button}>
        {t('repo.switchGo')}
      </button>
      {refs.data?.truncated === true && <span className={styles.hint}>{t('repo.refsTruncated')}</span>}
    </form>
  );
}

/** The path inside the repository as links to its folders. */
export function PathCrumbs({
  project,
  rev,
  path,
}: {
  project: string;
  rev: string;
  path: string;
}) {
  const { t } = useTranslation();
  const segments = path === '' ? [] : path.split('/');
  return (
    <nav aria-label={t('repo.pathLabel')} className={styles.crumbs}>
      <ol className={styles.crumbList}>
        <li>
          {segments.length === 0 ? (
            <span aria-current="page">{rev}</span>
          ) : (
            <Link to={treeUrl(project, rev)}>{rev}</Link>
          )}
        </li>
        {segments.map((segment, i) => {
          const sub = segments.slice(0, i + 1).join('/');
          const isLast = i === segments.length - 1;
          return (
            <li key={sub}>
              {isLast ? (
                <span aria-current="page">{segment}</span>
              ) : (
                <Link to={treeUrl(project, rev, sub)}>{segment}</Link>
              )}
            </li>
          );
        })}
      </ol>
    </nav>
  );
}

/** The ref to put in a raw / archive address: the short name, unless a
 * branch and a tag share it (the server reads a short name as the branch
 * first, so the tag would silently give the branch's files): then the commit
 * id, which names exactly one thing. */
export function exactRef(rev: Rev, refs: RefEntry[] | undefined): string {
  if (rev.type === 'commit') return rev.name;
  const same = (refs ?? []).filter((r) => r.name === rev.name);
  return same.length > 1 ? rev.commit : rev.name;
}

export function shortOid(oid: string): string {
  return oid.slice(0, 8);
}
