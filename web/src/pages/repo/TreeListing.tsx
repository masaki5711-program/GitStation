// One level of the tree, and the README of that level. The server lists at
// most 5000 entries and says so (`truncated`); an empty tree and a
// submodule (a commit in another repository, not a link) are said in words.
import { useTranslation } from 'react-i18next';
import { Link } from 'react-router';

import { refParam, useBlob, useTree, type Spec } from '../../api/repo';
import type { TreeEntry } from '../../api/types';
import { CodeView } from '../../features/CodeView';
import { MarkdownView } from '../../features/MarkdownView';
import { formatBytes } from '../../features/labels';
import { blobUrl, isMarkdownPath, parentPath, treeUrl } from '../../features/repoUrls';
import { Loading } from '../../components/Loading';
import { RepoError } from './common';
import styles from './Repo.module.css';

function order(entries: TreeEntry[]): TreeEntry[] {
  const rank = (e: TreeEntry) => (e.type === 'tree' ? 0 : e.type === 'submodule' ? 1 : 2);
  return [...entries].sort((a, b) => rank(a) - rank(b) || (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));
}

const README = /^readme(\.(md|markdown|txt))?$/i;

export function TreeListing({
  projectId,
  project,
  spec,
}: {
  projectId: number;
  project: string;
  spec: Spec;
}) {
  const { t, i18n } = useTranslation();
  const tree = useTree(projectId, refParam(spec.rev), spec.path);
  const name = spec.rev.name;
  if (tree.isPending) return <Loading />;
  if (tree.isError) {
    return (
      <RepoError
        error={tree.error}
        onRetry={() => {
          void tree.refetch();
        }}
      />
    );
  }
  const entries = order(tree.data.entries);
  const readme = entries.find((e) => e.type === 'blob' && README.test(e.name));
  return (
    <>
      {tree.data.truncated && (
        <p className={styles.warning} role="status" data-testid="tree-truncated">
          {t('tree.truncated', { count: tree.data.entries.length })}
        </p>
      )}
      <table className={styles.table}>
        <caption className={styles.srOnly}>{t('tree.caption')}</caption>
        <thead>
          <tr>
            <th scope="col">{t('tree.name')}</th>
            <th scope="col" className={styles.num}>
              {t('tree.size')}
            </th>
          </tr>
        </thead>
        <tbody>
          {spec.path !== '' && (
            <tr>
              <th scope="row">
                <Link to={treeUrl(project, name, parentPath(spec.path))}>{t('tree.parent')}</Link>
              </th>
              <td />
            </tr>
          )}
          {entries.map((entry) => (
            <tr key={entry.path}>
              <th scope="row" className={styles.entry}>
                {entry.type === 'tree' ? (
                  <Link to={treeUrl(project, name, entry.path)}>
                    {entry.name}
                    {'/'}
                  </Link>
                ) : entry.type === 'submodule' ? (
                  <span>
                    {entry.name} <span className={styles.badge}>{t('tree.submodule')}</span>
                  </span>
                ) : (
                  <Link to={blobUrl(project, name, entry.path)}>{entry.name}</Link>
                )}
                {entry.type === 'symlink' && (
                  <span className={styles.badge}>{t('tree.symlink')}</span>
                )}
              </th>
              <td className={styles.num}>
                {entry.size === null ? '' : formatBytes(entry.size, i18n.language)}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
      {entries.length === 0 && <p>{t('tree.empty')}</p>}
      {readme !== undefined && (
        <Readme projectId={projectId} project={project} spec={spec} path={readme.path} />
      )}
    </>
  );
}

function Readme({
  projectId,
  project,
  spec,
  path,
}: {
  projectId: number;
  project: string;
  spec: Spec;
  path: string;
}) {
  const { t } = useTranslation();
  const blob = useBlob(projectId, refParam(spec.rev), path);
  return (
    <section className={styles.readme} aria-labelledby="readme-title">
      <h2 id="readme-title" className={styles.sectionTitle}>
        <Link to={blobUrl(project, spec.rev.name, path)}>{path.split('/').pop()}</Link>
      </h2>
      {blob.isPending && <Loading />}
      {blob.isError && (
        <RepoError
          error={blob.error}
          onRetry={() => {
            void blob.refetch();
          }}
        />
      )}
      {blob.isSuccess &&
        (blob.data.binary || blob.data.content === null ? (
          <p>{t('file.binary')}</p>
        ) : (
          <>
            {blob.data.truncated && <p className={styles.warning}>{t('readme.truncated')}</p>}
            {isMarkdownPath(path) ? (
              <MarkdownView
                headingOffset={2}
                rawRef={spec.rev.commit}
                source={blob.data.content}
                project={project}
                refName={spec.rev.name}
                dir={parentPath(path)}
              />
            ) : (
              <CodeView path={path} text={blob.data.content} commit={spec.rev.commit} />
            )}
          </>
        ))}
    </section>
  );
}
