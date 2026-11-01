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
  MAX_VERSIONS,
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
  const { t, i18n } = useTranslation();
  const [params] = useSearchParams();
  const view = parseView(params);
  const latest = latestCommit(refs);
  const { items: all, cut } = useMemo(() => confirmedVersions(refs), [refs]);
  // The list and the pickers take the first MAX_VERSIONS; an address may name
  // any confirmed version, so those are looked up as well.
  const head = all.slice(0, MAX_VERSIONS);
  const named = all.filter(
    (c) => (c.name === view.version || c.name === view.compare) && !head.includes(c),
  );
  const commits = [
    ...(latest === null ? [] : [latest]),
    ...head.map((c) => c.commit),
    ...named.map((c) => c.commit),
  ];
  const moments = useMoments(projectId, commits);
  const momentOf = (commit: string) => moments[commits.indexOf(commit)]?.data ?? null;
  const toVersion = (c: { name: string; commit: string }) => ({
    name: c.name,
    commit: c.commit,
    at: momentOf(c.commit)?.at ?? null,
    message: momentOf(c.commit)?.message ?? null,
  });
  // Date order is applied before the cap, on the versions that were looked up.
  const listed = sortByDate(head.map(toVersion)).slice(0, MAX_VERSIONS);
  const pickable = sortByDate([...head, ...named].map(toVersion));
  const latestVersion: Version | null =
    latest === null
      ? null
      : { name: null, commit: latest, at: momentOf(latest)?.at ?? null, message: momentOf(latest)?.message ?? null };

  const requested = view.version === null ? undefined : pickable.find((v) => v.name === view.version);
  const selected: Version | undefined =
    view.version === null ? (latestVersion ?? pickable[0]) : requested;
  const older = view.compare === null ? undefined : pickable.find((v) => v.name === view.compare);
  const comparing = selected !== undefined && older !== undefined && older.commit !== selected.commit;

  if (latestVersion === null && pickable.length === 0) {
    return (
      <section aria-labelledby="portal-empty">
        <h2 id="portal-empty">{t('portal.empty.title')}</h2>
        <p>{t('portal.empty.body')}</p>
      </section>
    );
  }

  // One live region that is always in the page: it says what is shown, and
  // its text changes when the version does.
  let showing = '';
  if (selected !== undefined) {
    const label = versionName(selected, t('plain.latest'));
    showing = comparing
      ? t('portal.comparing', { from: versionName(older, t('plain.latest')), to: label })
      : t('portal.viewing', { version: label });
    if (selected.at !== null) showing += ` · ${formatDate(selected.at, i18n.language)}`;
    if (selected.message !== null && selected.message !== '') showing += ` · ${selected.message}`;
  }

  return (
    <>
      {refs.truncated && (
        <p className={styles.note} role="status">
          {t('portal.versionsTruncated')}
        </p>
      )}
      <p className={styles.showing} role="status">
        {showing}
      </p>
      <VersionsPanel
        project={project}
        view={view}
        latest={latestVersion}
        pickable={pickable}
        listed={listed}
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
          older={older}
          compareMissing={view.compare !== null && older === undefined}
        />
      )}
    </>
  );
}

function versionName(v: Version, latestLabel: string): string {
  return v.name ?? latestLabel;
}

/** The versions, collapsed: the document is what the page is for, so the
 * choice of version and the list of confirmed versions sit in one disclosure.
 * It is open on the first visit (nothing chosen yet) and closes once a
 * version has been chosen. */
function VersionsPanel({
  project,
  view,
  latest,
  pickable,
  listed,
  selected,
  cut,
}: {
  project: string;
  view: PortalView;
  latest: Version | null;
  pickable: Version[];
  listed: Version[];
  selected: string | null;
  cut: boolean;
}) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(view.version === null && view.compare === null);
  const first = useRef(true);
  useEffect(() => {
    if (first.current) {
      first.current = false;
      return;
    }
    setOpen(false);
  }, [view.version, view.compare]);
  return (
    <details
      className={styles.panel}
      open={open}
      onToggle={(e) => {
        setOpen(e.currentTarget.open);
      }}
    >
      <summary className={styles.summary}>{t('portal.versionsPanel')}</summary>
      <VersionPicker project={project} view={view} latest={latest} versions={pickable} />
      <ConfirmedList project={project} versions={listed} selected={selected} cut={cut} />
    </details>
  );
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
  // The form follows the address (Back, a link, a row's "Changes").
  useEffect(() => {
    setVersion(view.version ?? '');
    setCompare(view.compare ?? '');
  }, [view.version, view.compare]);
  const chosen = version === '' ? latest : versions.find((v) => v.name === version);
  // Only versions older than the chosen one can be "what changed since".
  const chosenAt = chosen?.at == null ? null : new Date(chosen.at).getTime();
  const olderOnes = versions.filter(
    (v) =>
      v.name !== version &&
      (v.name === compare ||
        chosenAt === null ||
        v.at === null ||
        new Date(v.at).getTime() < chosenAt),
  );
  const describe = (v: Version) =>
    [v.name ?? t('plain.latest'), v.at === null ? null : formatDate(v.at, i18n.language)]
      .filter((x): x is string => x !== null)
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
            {olderOnes.map((v) => (
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
      {chosen?.message != null && chosen.message !== '' && (
        <p className={styles.optionMessage}>{t('portal.messageLine', { message: chosen.message })}</p>
      )}
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
                  <tr key={v.commit + name} className={selected === name ? styles.rowShown : undefined}>
                    <th scope="row">
                      {name}
                      {i === 0 && <span className={styles.badgeConfirmed}>{t('portal.newestConfirmed')}</span>}
                      {selected === name && <span className={styles.badgeShowing}>{t('portal.showing')}</span>}
                    </th>
                    <td>{v.at === null ? '' : formatDate(v.at, i18n.language)}</td>
                    <td>{v.message ?? ''}</td>
                    <td>
                      <span className={styles.rowActions}>
                        <Link
                          className={styles.link}
                          to={portalUrl(project, { version: name })}
                          aria-label={t('portal.openOf', { name })}
                        >
                          {t('portal.open')}
                        </Link>
                        {previous?.name != null && (
                          <Link
                            className={styles.link}
                            to={portalUrl(project, { version: name, compare: previous.name })}
                            aria-label={t('portal.changesOf', { name, previous: previous.name })}
                          >
                            {t('portal.changes')}
                          </Link>
                        )}
                        <a
                          className={styles.link}
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
  const { t } = useTranslation();
  const comparing = older !== undefined && older.commit !== selected.commit;
  const chapters = useChapters(projectId, selected.commit);
  const olderChapters = useChapters(projectId, comparing ? older.commit : null);
  const compare = useCompare(
    projectId,
    comparing ? older.commit : null,
    comparing ? selected.commit : null,
  );
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

  // After a new version or comparison is chosen the reader's heading takes
  // the focus (the control that was used may be gone). A change of chapter
  // alone is the chapter heading's business (ChapterPane).
  const heading = useRef<HTMLHeadingElement>(null);
  const firstRender = useRef(true);
  const versionKey = `${selected.commit}|${comparing ? older.commit : ''}`;
  useEffect(() => {
    if (firstRender.current) {
      firstRender.current = false;
      return;
    }
    heading.current?.focus();
  }, [versionKey]);

  return (
    <section aria-labelledby="portal-reader" className={styles.section}>
      <h2 id="portal-reader" ref={heading} tabIndex={-1} className={styles.sectionTitle}>
        {comparing ? t('portal.differences') : t('portal.chapters')}
      </h2>
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
              <Toc list={list} current={current} comparing={comparing} others={others} link={link} />
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

/** The chapter list. Beside the text on a wide screen (it stays in view while
 * the text scrolls); folded away on a narrow one. A change's label is part of
 * the link, so a screen reader hears "01-scope Changed". */
function Toc({
  list,
  current,
  comparing,
  others,
  link,
}: {
  list: ChapterChange[];
  current: ChapterChange | undefined;
  comparing: boolean;
  others: number;
  link: (chapter: string | null) => string;
}) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(
    typeof window.matchMedia !== 'function' || !window.matchMedia('(max-width: 48rem)').matches,
  );
  return (
    <details
      className={styles.toc}
      open={open}
      onToggle={(e) => {
        setOpen(e.currentTarget.open);
      }}
    >
      <summary className={styles.summary}>{t('portal.chapterList')}</summary>
      <nav aria-label={t('portal.chapterList')}>
        <ol>
          {list.map((c) => (
            <li key={c.chapter.path}>
              <Link
                className={styles.link}
                to={link(c.chapter.path)}
                {...(current?.chapter.path === c.chapter.path ? { 'aria-current': 'page' as const } : {})}
              >
                {c.chapter.title}
                {comparing && c.kind !== 'same' && (
                  <span className={styles.badge}>
                    {c.kind === 'added'
                      ? t('portal.compare.added')
                      : c.kind === 'removed'
                        ? t('portal.compare.removed')
                        : t('portal.compare.changed')}
                  </span>
                )}
              </Link>
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
    </details>
  );
}
