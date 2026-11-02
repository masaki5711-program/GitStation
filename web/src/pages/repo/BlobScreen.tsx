// A file. Every state is said in words: text (coloured, or Markdown drawn),
// cut at 1 MiB (`truncated`), binary (an image from the raw route when it is
// a png / jpeg / gif / webp; otherwise a download), formats the screen never
// draws (SVG, HTML, XML, PDF: ADR-0015, download only), a symlink.
import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Link, Navigate } from 'react-router';

import { refParam, useBlob } from '../../api/repo';
import type { Rev } from '../../api/types';
import { Loading } from '../../components/Loading';
import controls from '../../components/controls.module.css';
import { CodeView } from '../../features/CodeView';
import { MarkdownView } from '../../features/MarkdownView';
import { formatBytes } from '../../features/labels';
import {
  baseName,
  blameUrl,
  commitsUrl,
  isDownloadOnly,
  isImagePath,
  isMarkdownPath,
  parentPath,
  rawUrl,
  treeUrl,
} from '../../features/repoUrls';
import { useDocumentTitle } from '../../hooks/useDocumentTitle';
import { PathCrumbs, RepoError, shortOid, WithSpec } from './common';
import styles from './Repo.module.css';

export function BlobScreen({
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
      {(s) => <Blob projectId={projectId} project={project} rev={s.rev} path={s.path} />}
    </WithSpec>
  );
}

function Blob({
  projectId,
  project,
  rev,
  path,
}: {
  projectId: number;
  project: string;
  rev: Rev;
  path: string;
}) {
  const { t, i18n } = useTranslation();
  const blob = useBlob(projectId, refParam(rev), path, path !== '');
  const [view, setView] = useState<'rendered' | 'source'>('rendered');
  useDocumentTitle(path === '' ? null : baseName(path));

  // A folder address (the root) is the tree screen.
  if (path === '') return <Navigate replace to={treeUrl(project, rev.name)} />;
  if (blob.isPending) return <Loading />;
  if (blob.isError) {
    return (
      <RepoError
        error={blob.error}
        onRetry={() => {
          void blob.refetch();
        }}
      />
    );
  }
  const file = blob.data;
  // The commit id names exactly one version, whatever the short name is.
  const raw = rawUrl(project, rev.commit, path);
  const text = file.content;
  const image = file.binary && isImagePath(path);
  const markdown = isMarkdownPath(path) && text !== null && file.type === 'blob';

  return (
    <>
      <PathCrumbs project={project} rev={rev.name} path={path} />
      <h1 className={styles.title}>{baseName(path)}</h1>
      <p className={styles.meta}>
        <span>{formatBytes(file.size, i18n.language)}</span>
        <span>
          {t('file.mode')} <code>{file.mode}</code>
        </span>
        <span>
          {t('file.object')} <code>{shortOid(file.oid)}</code>
        </span>
      </p>
      <p className={styles.actions}>
        <a className={controls.button} href={raw}>
          {t('file.raw')}
        </a>
        <a className={controls.button} href={raw} download={baseName(path)}>
          {t('file.download')}
        </a>
        {!file.binary && file.type === 'blob' && (
          <Link className={controls.button} to={blameUrl(project, rev.name, path)}>
            {t('file.blame')}
          </Link>
        )}
        <Link className={controls.button} to={commitsUrl(project, rev.name, path)}>
          {t('file.history')}
        </Link>
        {markdown && (
          <span className={styles.toggle} role="group" aria-label={t('file.viewLabel')}>
            <button
              type="button"
              className={controls.button}
              aria-pressed={view === 'rendered'}
              onClick={() => {
                setView('rendered');
              }}
            >
              {t('file.rendered')}
            </button>
            <button
              type="button"
              className={controls.button}
              aria-pressed={view === 'source'}
              onClick={() => {
                setView('source');
              }}
            >
              {t('file.source')}
            </button>
          </span>
        )}
      </p>

      {file.truncated && (
        <p className={styles.warning} role="status" data-testid="blob-truncated">
          {t('file.truncated', { size: formatBytes(file.size, i18n.language) })}
        </p>
      )}
      {isDownloadOnly(path) && (
        <p className={styles.warning} role="status" data-testid="download-only">
          {t('file.downloadOnly')}
        </p>
      )}
      {file.type === 'symlink' && text !== null && (
        <p data-testid="symlink">
          {t('file.symlinkTo')} <code>{text}</code>
        </p>
      )}

      {image && (
        <p>
          <img className={styles.image} src={raw} alt={baseName(path)} />
        </p>
      )}
      {file.binary && !image && (
        <p className={styles.warning} role="status" data-testid="binary">
          {t('file.binary')}
        </p>
      )}
      {!file.binary && file.type === 'blob' && text !== null && (
        <>
          {text === '' ? (
            <p>{t('file.emptyFile')}</p>
          ) : markdown && view === 'rendered' ? (
            <MarkdownView
              source={text}
              project={project}
              refName={rev.name}
              rawRef={rev.commit}
              dir={parentPath(path)}
              headingOffset={1}
            />
          ) : (
            <CodeView path={path} text={text} commit={rev.commit} />
          )}
        </>
      )}
    </>
  );
}
