// The document portal for one specification (plan 10 §4.7), read-only:
// chapters on the left, the chapter on the right, a choice of version, the
// differences between two versions chapter by chapter, and downloads. The
// screen speaks the words of 08 (specification, confirmed version, work in
// progress); a test keeps the words of the version control system out of it
// in every language.
import { useEffect, useId, useMemo, useRef, useState, type SyntheticEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { Link, useNavigate, useSearchParams } from 'react-router';

import { useChapters, useMoments, type Chapter } from '../../api/portal';
import { useBlob, useCompare, useRefs } from '../../api/repo';
import { Loading } from '../../components/Loading';
import controls from '../../components/controls.module.css';
import { MarkdownView } from '../../features/MarkdownView';
import { formatBytes, formatDate } from '../../features/labels';
import {
  changesPerChapter,
  confirmedVersions,
  latestCommit,
  parseView,
  portalUrl,
  sortByDate,
  type ChapterChange,
  type PortalView,
} from '../../features/portal';
import { archiveUrl, baseName, encodePath, rawUrl } from '../../features/repoUrls';
import { useDocumentTitle } from '../../hooks/useDocumentTitle';
import { RepoError } from '../repo/common';
import { ChapterChanges } from './ChapterChanges';
import styles from './Portal.module.css';

/** One version the reader can open. `name` null: the latest work in progress. */
interface Version {
  name: string | null;
  commit: string;
  at: string | null;
  message: string | null;
}

export function DocsScreen({
  projectId,
  project,
  displayName,
}: {
  projectId: number;
  project: string;
  displayName: string;
}) {
  const { t } = useTranslation();
  const refs = useRefs(projectId);
  useDocumentTitle(`${displayName} · ${t('plain.spec')}`);
  return (
    <>
      <p className={styles.top}>
        <Link to={`/${encodePath(project)}`}>{t('portal.backToProject')}</Link>
        <Link to={`/${encodePath(project)}/-/tree`}>{t('portal.technicalView')}</Link>
      </p>
      <h1 className={styles.title}>
        {displayName} <span className={styles.titleNote}>{t('plain.spec')}</span>
      </h1>
      {refs.isPending && <Loading />}
      {refs.isError && (
        <RepoError
          error={refs.error}
          onRetry={() => {
            void refs.refetch();
          }}
        />
      )}
      {refs.isSuccess && (
        <Loaded projectId={projectId} project={project} refs={refs.data} />
      )}
    </>
  );
}

function Loaded({
  projectId,
  project,
  refs,
}: {
  projectId: number;
  project: string;
  refs: NonNullable<ReturnType<typeof useRefs>['data']>;
}) {
  const { t } = useTranslation();
  const [params] = useSearchParams();
  const view = parseView(params);
  const latest = latestCommit(refs);
  const { items: confirmed, cut } = useMemo(() => confirmedVersions(refs), [refs]);
  const commits = [...(latest === null ? [] : [latest]), ...confirmed.map((c) => c.commit)];
  const moments = useMoments(projectId, commits);
  const momentOf = (commit: string) => {
    const i = commits.indexOf(commit);
    return moments[i]?.data ?? null;
  };

  const versions: Version[] = sortByDate(
    confirmed.map((c) => ({
      name: c.name,
      commit: c.commit,
      at: momentOf(c.commit)?.at ?? null,
      message: momentOf(c.commit)?.message ?? null,
    })),
  );
  const latestVersion: Version | null =
    latest === null
      ? null
      : { name: null, commit: latest, at: momentOf(latest)?.at ?? null, message: momentOf(latest)?.message ?? null };

  if (latestVersion === null && versions.length === 0) {
    return (
      <section aria-labelledby="portal-empty">
        <h2 id="portal-empty">{t('portal.empty.title')}</h2>
        <p>{t('portal.empty.body')}</p>
      </section>
    );
  }

  const requested = view.version === null ? undefined : versions.find((v) => v.name === view.version);
  const selected: Version | undefined =
    view.version === null ? (latestVersion ?? versions[0]) : requested;
  const olderRequested =
    view.compare === null ? undefined : versions.find((v) => v.name === view.compare);

  return (
    <>
      {refs.truncated && (
        <p className={styles.note} role="status">
          {t('portal.versionsTruncated')}
        </p>
      )}
      <VersionPicker
        key={`${view.version ?? ''}|${view.compare ?? ''}`}
        project={project}
        view={view}
        latest={latestVersion}
        versions={versions}
      />
      <ConfirmedList
        project={project}
        versions={versions}
        selected={selected?.name ?? null}
        cut={cut}
      />
      {selected === undefined ? (
        <section aria-labelledby="portal-missing">
          <h2 id="portal-missing">{t('portal.versionMissing.title')}</h2>
          <p>{t('portal.versionMissing.body')}</p>
          <p>
            <Link to={portalUrl(project)}>{t('portal.versionMissing.latest')}</Link>
          </p>
        </section>
      ) : (
        <Reader
          projectId={projectId}
          project={project}
          view={view}
          selected={selected}
          older={olderRequested}
          compareMissing={view.compare !== null && olderRequested === undefined}
        />
      )}
    </>
  );
}

function versionName(v: Version, latestLabel: string): string {
  return v.name ?? latestLabel;
}

function VersionPicker({
  project,
  view,
  latest,
  versions,
}: {
  project: string;
  view: PortalView;
  latest: Version | null;
  versions: Version[];
}) {
  const { t, i18n } = useTranslation();
  const navigate = useNavigate();
  const versionId = useId();
  const compareId = useId();
  const [version, setVersion] = useState(view.version ?? '');
  const [compare, setCompare] = useState(view.compare ?? '');
  const describe = (v: Version) =>
    [v.name ?? t('plain.latest'), v.at === null ? null : formatDate(v.at, i18n.language), v.message]
      .filter((x): x is string => x !== null && x !== '')
      .join(' — ');
  const submit = (e: SyntheticEvent) => {
    e.preventDefault();
    void navigate(
      portalUrl(project, {
        version: version === '' ? null : version,
        compare: compare === '' || compare === version ? null : compare,
      }),
    );
  };
  return (
    <section aria-labelledby="portal-versions" className={styles.section}>
      <h2 id="portal-versions" className={styles.sectionTitle}>
        {t('plain.version')}
      </h2>
      <form onSubmit={submit} className={styles.pickerForm}>
        <div className={styles.field}>
          <label htmlFor={versionId}>{t('portal.showVersion')}</label>
          <select
            id={versionId}
            className={controls.input}
            value={version}
            onChange={(e) => {
              setVersion(e.target.value);
            }}
          >
            {latest !== null && (
              <optgroup label={t('plain.working')}>
                <option value="">{describe(latest)}</option>
              </optgroup>
            )}
            {versions.length > 0 && (
              <optgroup label={t('plain.confirmed')}>
                {versions.map((v) => (
                  <option key={v.commit + (v.name ?? '')} value={v.name ?? ''}>
                    {describe(v)}
                  </option>
                ))}
              </optgroup>
            )}
          </select>
        </div>
        <div className={styles.field}>
          <label htmlFor={compareId}>{t('portal.compareWith')}</label>
          <select
            id={compareId}
            className={controls.input}
            value={compare}
            onChange={(e) => {
              setCompare(e.target.value);
            }}
          >
            <option value="">{t('portal.compareNone')}</option>
            {versions
              .filter((v) => v.name !== version)
              .map((v) => (
                <option key={v.commit + (v.name ?? '')} value={v.name ?? ''}>
                  {describe(v)}
                </option>
              ))}
          </select>
        </div>
        <button type="submit" className={controls.primary}>
          {t('portal.show')}
        </button>
      </form>
    </section>
  );
}

function ConfirmedList({
  project,
  versions,
  selected,
  cut,
}: {
  project: string;
  versions: Version[];
  selected: string | null;
  cut: boolean;
}) {
  const { t, i18n } = useTranslation();
  return (
    <section aria-labelledby="portal-confirmed" className={styles.section}>
      <h2 id="portal-confirmed" className={styles.sectionTitle}>
        {t('plain.confirmed')}
      </h2>
      {versions.length === 0 ? (
        <p>{t('portal.noConfirmed')}</p>
      ) : (
        <div className={styles.scroll} role="region" aria-label={t('plain.confirmed')} tabIndex={0}>
          <table className={styles.table}>
            <thead>
              <tr>
                <th scope="col">{t('plain.version')}</th>
                <th scope="col">{t('portal.confirmedOn')}</th>
                <th scope="col">{t('portal.message')}</th>
                <th scope="col">{t('common.actions')}</th>
              </tr>
            </thead>
            <tbody>
              {versions.map((v, i) => {
                const name = v.name ?? '';
                const previous = versions[i + 1];
                return (
                  <tr key={v.commit + name}>
                    <th scope="row">
                      {name}
                      {i === 0 && <span className={styles.badge}>{t('portal.newestConfirmed')}</span>}
                      {selected === name && <span className={styles.badge}>{t('portal.showing')}</span>}
                    </th>
                    <td>{v.at === null ? '' : formatDate(v.at, i18n.language)}</td>
                    <td>{v.message ?? ''}</td>
                    <td>
                      <span className={styles.rowActions}>
                        <Link
                          to={portalUrl(project, { version: name })}
                          aria-label={t('portal.openOf', { name })}
                        >
                          {t('portal.open')}
                        </Link>
                        {previous?.name != null && (
                          <Link
                            to={portalUrl(project, { version: name, compare: previous.name })}
                            aria-label={t('portal.changesOf', { name, previous: previous.name })}
                          >
                            {t('portal.changes')}
                          </Link>
                        )}
                        <a
                          href={archiveUrl(project, v.commit, 'zip')}
                          aria-label={t('portal.downloadVersionOf', { name })}
                        >
                          {t('portal.downloadVersion')}
                        </a>
                      </span>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
      {cut && <p className={styles.note}>{t('portal.listCut', { count: versions.length })}</p>}
    </section>
  );
}

function Reader({
  projectId,
  project,
  view,
  selected,
  older,
  compareMissing,
}: {
  projectId: number;
  project: string;
  view: PortalView;
  selected: Version;
  older: Version | undefined;
  compareMissing: boolean;
}) {
  const { t, i18n } = useTranslation();
  const comparing = older !== undefined && older.commit !== selected.commit;
  const chapters = useChapters(projectId, selected.commit);
  const olderChapters = useChapters(projectId, comparing ? older.commit : null);
  const compare = useCompare(
    projectId,
    comparing ? older.commit : null,
    comparing ? selected.commit : null,
  );
  const label = versionName(selected, t('plain.latest'));

  const pending = chapters.isPending || (comparing && (olderChapters.isPending || compare.isPending));
  const failure = chapters.error ?? (comparing ? (olderChapters.error ?? compare.error) : null);

  let list: ChapterChange[] = [];
  let others = 0;
  if (chapters.isSuccess) {
    if (comparing && olderChapters.isSuccess && compare.isSuccess) {
      const r = changesPerChapter(olderChapters.data.chapters, chapters.data.chapters, compare.data.diff);
      list = r.chapters;
      others = r.others;
    } else {
      list = chapters.data.chapters.map((chapter) => ({ chapter, kind: 'same' as const, file: null }));
    }
  }
  const current: ChapterChange | undefined =
    view.chapter !== null
      ? list.find((c) => c.chapter.path === view.chapter)
      : (list.find((c) => comparing && c.kind !== 'same') ?? list[0]);

  const link = (chapter: string | null) =>
    portalUrl(project, {
      version: selected.name,
      compare: comparing ? older.name : null,
      chapter,
    });

  const lead = (
    <p className={styles.showing} role="status">
      {comparing
        ? t('portal.comparing', { from: versionName(older, t('plain.latest')), to: label })
        : t('portal.viewing', { version: label })}
      {selected.at !== null && ` · ${formatDate(selected.at, i18n.language)}`}
      {selected.message !== null && selected.message !== '' && ` · ${selected.message}`}
    </p>
  );

  return (
    <section aria-labelledby="portal-reader" className={styles.section}>
      <h2 id="portal-reader" className={styles.sectionTitle}>
        {comparing ? t('portal.differences') : t('portal.chapters')}
      </h2>
      {lead}
      <p className={styles.actions}>
        <a className={controls.button} href={archiveUrl(project, selected.commit, 'zip')}>
          {t('portal.downloadAll')}
        </a>
        {comparing && (
          <Link className={controls.button} to={portalUrl(project, { version: selected.name })}>
            {t('portal.stopComparing')}
          </Link>
        )}
      </p>
      {compareMissing && (
        <p className={styles.note} role="status">
          {t('portal.compareMissing')}
        </p>
      )}
      {pending && <Loading />}
      {!pending && failure !== null && <RepoError error={failure} />}
      {!pending && failure === null && chapters.isSuccess && (
        <>
          {chapters.data.truncated && (
            <p className={styles.note} role="status">
              {t('portal.chaptersCut')}
            </p>
          )}
          {comparing && compare.isSuccess && compare.data.diff.truncated && (
            <p className={styles.note} role="status">
              {t('portal.compare.truncated')}
            </p>
          )}
          {list.length === 0 ? (
            <p>{t('portal.noChapters')}</p>
          ) : (
            <div className={styles.layout}>
              <nav aria-label={t('portal.chapterList')} className={styles.toc}>
                <ol>
                  {list.map((c) => (
                    <li key={c.chapter.path}>
                      <Link
                        to={link(c.chapter.path)}
                        {...(current?.chapter.path === c.chapter.path
                          ? { 'aria-current': 'page' as const }
                          : {})}
                      >
                        {c.chapter.title}
                      </Link>
                      {comparing && c.kind !== 'same' && (
                        <span className={styles.badge}>
                          {c.kind === 'added'
                            ? t('portal.compare.added')
                            : c.kind === 'removed'
                              ? t('portal.compare.removed')
                              : t('portal.compare.changed')}
                        </span>
                      )}
                    </li>
                  ))}
                </ol>
                {comparing && (
                  <p className={styles.tocNote}>
                    {t('portal.compare.count', { count: list.filter((c) => c.kind !== 'same').length })}
                    {others > 0 && ` ${t('portal.compare.others', { count: others })}`}
                  </p>
                )}
              </nav>
              <div className={styles.content}>
                {current === undefined ? (
                  <>
                    <p>{t('portal.chapterMissing')}</p>
                    <p>
                      <Link to={link(null)}>{t('portal.firstChapter')}</Link>
                    </p>
                  </>
                ) : (
                  <ChapterPane
                    projectId={projectId}
                    project={project}
                    selected={selected}
                    versionName={selected.name}
                    change={current}
                    comparing={comparing}
                    chapters={list.map((c) => c.chapter)}
                    bodyLink={portalUrl(project, { version: selected.name, chapter: current.chapter.path })}
                  />
                )}
              </div>
            </div>
          )}
        </>
      )}
    </section>
  );
}

function ChapterPane({
  projectId,
  project,
  selected,
  versionName,
  change,
  comparing,
  chapters,
  bodyLink,
}: {
  projectId: number;
  project: string;
  selected: Version;
  versionName: string | null;
  change: ChapterChange;
  comparing: boolean;
  chapters: Chapter[];
  bodyLink: string;
}) {
  const { t } = useTranslation();
  const heading = useRef<HTMLHeadingElement>(null);
  const first = useRef(true);
  const path = change.chapter.path;
  // A chapter chosen from the list takes the reader to its heading.
  useEffect(() => {
    if (first.current) {
      first.current = false;
      return;
    }
    heading.current?.focus();
  }, [path]);
  const raw = rawUrl(project, selected.commit, path);
  return (
    <article aria-labelledby="portal-chapter">
      <h3 id="portal-chapter" ref={heading} tabIndex={-1} className={styles.chapterTitle}>
        {change.chapter.title}
      </h3>
      <p className={styles.actions}>
        {change.kind !== 'removed' && (
          <a className={controls.button} href={raw} download={baseName(path)}>
            {t('portal.downloadChapter')}
          </a>
        )}
        {comparing && change.kind !== 'removed' && (
          <Link className={controls.button} to={bodyLink}>
            {t('portal.readChapter')}
          </Link>
        )}
      </p>
      {comparing ? (
        <>
          <p className={styles.legend}>{t('portal.compare.legend')}</p>
          <ChapterChanges kind={change.kind} file={change.file} />
        </>
      ) : (
        <ChapterBody
          projectId={projectId}
          project={project}
          selected={selected}
          versionName={versionName}
          path={path}
          chapters={chapters}
          rawHref={raw}
        />
      )}
    </article>
  );
}

function ChapterBody({
  projectId,
  project,
  selected,
  versionName,
  path,
  chapters,
  rawHref,
}: {
  projectId: number;
  project: string;
  selected: Version;
  versionName: string | null;
  path: string;
  chapters: Chapter[];
  rawHref: string;
}) {
  const { t, i18n } = useTranslation();
  const blob = useBlob(projectId, selected.commit, path);
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
  if (file.binary || file.content === null) {
    return (
      <p className={styles.note} role="status" data-testid="chapter-binary">
        {t('portal.chapterBinary', { size: formatBytes(file.size, i18n.language) })}{' '}
        <a href={rawHref} download={baseName(path)}>
          {t('portal.downloadChapter')}
        </a>
      </p>
    );
  }
  const dir = path.includes('/') ? path.slice(0, path.lastIndexOf('/')) : '';
  const known = new Set(chapters.map((c) => c.path));
  return (
    <>
      {file.truncated && (
        <p className={styles.note} role="status" data-testid="chapter-truncated">
          {t('portal.chapterCut')}
        </p>
      )}
      {file.content === '' ? (
        <p>{t('portal.chapterEmpty')}</p>
      ) : (
        <MarkdownView
          source={file.content}
          project={project}
          refName={selected.commit}
          rawRef={selected.commit}
          dir={dir}
          headingOffset={3}
          linkTo={(target, folder) =>
            !folder && known.has(target) ? portalUrl(project, { version: versionName, chapter: target }) : null
          }
        />
      )}
    </>
  );
}
