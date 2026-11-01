// The files of a ref: where you are, the ref switcher, the download links
// (zip / tar.gz of the ref, built by the server), the clone URL and the
// listing with the README.
import { useTranslation } from 'react-i18next';

import type { Spec } from '../../api/repo';
import { CopyButton } from '../../components/CopyButton';
import { archiveUrl, treeUrl } from '../../features/repoUrls';
import { useDocumentTitle } from '../../hooks/useDocumentTitle';
import { PathCrumbs, RefSwitcher, WithSpec } from './common';
import { TreeListing } from './TreeListing';
import styles from './Repo.module.css';

export function TreeScreen({
  projectId,
  project,
  spec,
}: {
  projectId: number;
  project: string;
  spec: string;
}) {
  return (
    <WithSpec projectId={projectId} project={project} spec={spec}>
      {(s) => (
        <Files
          projectId={projectId}
          project={project}
          rev={s.rev.name}
          spec={s}
        />
      )}
    </WithSpec>
  );
}

function Files({
  projectId,
  project,
  rev,
  spec,
}: {
  projectId: number;
  project: string;
  rev: string;
  spec: Spec;
}) {
  const { t } = useTranslation();
  const folder = spec.path === '' ? t('tree.root') : (spec.path.split('/').pop() ?? '');
  useDocumentTitle(spec.path === '' ? t('tree.title') : folder);
  const cloneUrl = `${window.location.origin}/${project}.git`;
  return (
    <>
      <PathCrumbs project={project} rev={rev} path={spec.path} />
      <h1 className={styles.title}>{spec.path === '' ? t('tree.title') : folder}</h1>
      <div className={styles.toolbar}>
        <RefSwitcher projectId={projectId} current={rev} to={(ref) => treeUrl(project, ref)} />
        <p className={styles.links}>
          <a href={archiveUrl(project, rev, 'zip')}>{t('tree.downloadZip')}</a>
          <a href={archiveUrl(project, rev, 'tar.gz')}>{t('tree.downloadTar')}</a>
        </p>
      </div>
      <div className={styles.cloneLine}>
        <code>{cloneUrl}</code>
        <CopyButton text={cloneUrl} label={t('project.copyClone')} />
      </div>
      <TreeListing projectId={projectId} project={project} spec={spec} />
    </>
  );
}
