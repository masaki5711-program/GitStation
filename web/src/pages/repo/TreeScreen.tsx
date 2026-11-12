// The files of a ref: where you are, the ref switcher, the download links
// (zip / tar.gz of the ref, built by the server), the clone URL and the
// listing with the README.
import { useTranslation } from 'react-i18next';
import { Link } from 'react-router';

import { useRefs } from '../../api/repo';

import type { Spec } from '../../api/repo';
import { CopyButton } from '../../components/CopyButton';
import controls from '../../components/controls.module.css';
import { archiveUrl, newFileUrl, treeUrl } from '../../features/repoUrls';
import { useDocumentTitle } from '../../hooks/useDocumentTitle';
import { exactRef, PathCrumbs, RefSwitcher, WithSpec } from './common';
import { TreeListing } from './TreeListing';
import styles from './Repo.module.css';
import { PageHeader } from '../../ui/PageHeader';

export function TreeScreen({
  projectId,
  project,
  spec,
  canPush = false,
}: {
  projectId: number;
  project: string;
  spec: string;
  /** The person may push (Developer or above): offer a new file. */
  canPush?: boolean;
}) {
  return (
    <WithSpec projectId={projectId} project={project} spec={spec}>
      {(s) => <Files projectId={projectId} project={project} spec={s} canPush={canPush} />}
    </WithSpec>
  );
}

function Files({
  projectId,
  project,
  spec,
  canPush,
}: {
  projectId: number;
  project: string;
  spec: Spec;
  canPush: boolean;
}) {
  const { t } = useTranslation();
  const refs = useRefs(projectId);
  const rev = spec.rev.name;
  const archiveRef = exactRef(spec.rev, refs.data?.refs);
  const folder = spec.path === '' ? t('tree.root') : (spec.path.split('/').pop() ?? '');
  useDocumentTitle(spec.path === '' ? t('tree.title') : folder);
  const cloneUrl = `${window.location.origin}/${project}.git`;
  return (
    <>
      <PathCrumbs project={project} rev={rev} path={spec.path} />
      <PageHeader
        title={spec.path === '' ? t('tree.title') : folder}
        actions={
          canPush && spec.rev.type === 'branch' ? (
            <Link className={controls.primary} to={newFileUrl(project, rev, spec.path)}>
              {t('tree.newFile')}
            </Link>
          ) : undefined
        }
      />
      <div className={styles.toolbar}>
        <RefSwitcher projectId={projectId} current={rev} to={(ref) => treeUrl(project, ref)} />
        <p className={styles.links}>
          <a href={archiveUrl(project, archiveRef, 'zip')}>{t('tree.downloadZip')}</a>
          <a href={archiveUrl(project, archiveRef, 'tar.gz')}>{t('tree.downloadTar')}</a>
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
