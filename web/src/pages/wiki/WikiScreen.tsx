// The project's "Wiki" tab (`/{project}/-/wiki[/…]`, plan 20 W4, ADR-0040
// §5). The wiki is an ordinary project linked to this one; its pages are
// plan 13 documents of type `wiki.page`, read through the document API and
// saved by the simple editor's one write as a direct commit to the wiki's
// default branch (its protection lets a Developer push; Q3).
//
//   /-/wiki                 the pages as a folder tree, or "Wiki を作る"
//   /-/wiki/new             a new page (the form; never YAML)
//   /-/wiki/{ID}            one page
//   /-/wiki/{ID}/edit       the page in the form
//   /-/wiki/{ID}/history    the commits of the page's file
//
// When the lint is red (the head's `forge/doc-lint`, or the page's own
// parse problems) one line says where and links "直す" (plan 08).
import { useInfiniteQuery, useQuery, useQueryClient } from '@tanstack/react-query';
import { useEffect, useId, useState, type ReactNode, type SyntheticEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { Link, useNavigate } from 'react-router';

import { failureOf } from '../../api/client';
import { useApi } from '../../api/context';
import { isDocId, useDocument } from '../../api/documents';
import { CODE_CONFLICT, commitErrorCode, mayPush, useSingleFileCommit } from '../../api/fileCommits';
import { projectQuery } from '../../api/namespaces';
import { commitsQuery, useBlob } from '../../api/repo';
import type { DocumentBody } from '../../api/types';
import { useCreateWiki, useWikiChecks, useWikiLink, useWikiPages, type WikiLink } from '../../api/wiki';
import { ErrorNotice } from '../../components/ErrorNotice';
import { SelectField, TextField } from '../../components/Field';
import { Loading } from '../../components/Loading';
import controls from '../../components/controls.module.css';
import { linkRefs, docRefTarget, type RefWords } from '../../features/docRefs';
import { formatDateTime } from '../../features/labels';
import { MarkdownView } from '../../features/MarkdownView';
import { blobUrl, commitUrl, encodePath, parentPath, treeUrl } from '../../features/repoUrls';
import {
  CLASSIFICATIONS,
  cleanFolder,
  composePage,
  nextWikiId,
  ownerOf,
  pageTree,
  parsePage,
  type Classification,
  type PageEntry,
  type PageTree,
  type WikiPage,
} from '../../features/wikiPage';
import { useDocumentTitle } from '../../hooks/useDocumentTitle';
import { useSession } from '../../session/session';
import { EmptyState } from '../../ui/EmptyState';
import { PageHeader } from '../../ui/PageHeader';
import { NotFound } from '../NotFound';
import { RepoError } from '../repo/common';
import styles from './Wiki.module.css';

/** The check whose red the wiki's one line explains. */
const DOC_LINT = 'forge/doc-lint';
const RED = ['failure', 'error', 'action_required'];

interface Ctx {
  link: WikiLink;
  /** `/{project}/-/wiki` of the project the tab is on. */
  base: string;
  canEdit: boolean;
}

export function WikiScreen({
  projectId,
  project,
  role,
  view,
}: {
  projectId: number;
  /** Canonical path of the project the tab is on. */
  project: string;
  /** The caller's role on that project (who may create the wiki). */
  role: string | null | undefined;
  /** The segments after `/-/wiki`. */
  view: string[];
}) {
  const { t } = useTranslation();
  const api = useApi();
  const isAdmin = useSession().user.is_admin;
  const link = useWikiLink(projectId);
  const wikiProject = useQuery({
    ...projectQuery(api, link.data?.wiki_project_id ?? 0),
    enabled: link.data != null,
  });
  useDocumentTitle(t('wiki.title'));
  const base = `/${encodePath(project)}/-/wiki`;
  const [first, second, ...rest] = view;
  const shapeOk =
    view.length === 0 ||
    (view.length === 1 && (first === 'new' || isDocId(first ?? ''))) ||
    (view.length === 2 && isDocId(first ?? '') && (second === 'edit' || second === 'history'));
  if (!shapeOk || rest.length > 0) return <NotFound />;
  if (link.isPending) return <Loading />;
  if (link.isError) {
    return (
      <>
        <PageHeader title={t('wiki.title')} />
        <RepoError
          error={link.error}
          onRetry={() => {
            void link.refetch();
          }}
        />
      </>
    );
  }
  if (link.data === null) {
    const canCreate = isAdmin || role === 'maintainer' || role === 'owner';
    return <NoWiki projectId={projectId} canCreate={canCreate} />;
  }
  if (wikiProject.isPending) return <Loading />;
  const ctx: Ctx = {
    link: link.data,
    base,
    canEdit: wikiProject.isSuccess && mayPush(wikiProject.data.role, isAdmin),
  };
  if (view.length === 0) return <WikiHome ctx={ctx} />;
  if (first === 'new') return ctx.canEdit ? <NewPageForm ctx={ctx} /> : <NotFound />;
  const docId = first ?? '';
  if (second === 'edit') return ctx.canEdit ? <EditPageForm key={docId} ctx={ctx} docId={docId} /> : <NotFound />;
  if (second === 'history') return <PageHistory ctx={ctx} docId={docId} />;
  return <PageView ctx={ctx} docId={docId} />;
}

function NoWiki({ projectId, canCreate }: { projectId: number; canCreate: boolean }) {
  const { t } = useTranslation();
  const create = useCreateWiki(projectId);
  return (
    <section aria-labelledby="wiki-title">
      <PageHeader title={t('wiki.title')} titleId="wiki-title" />
      <EmptyState
        icon="folder"
        reason={t('wiki.empty.reason')}
        hints={[t('wiki.empty.hint'), ...(canCreate ? [] : [t('wiki.empty.whoCreates')])]}
        action={
          canCreate ? (
            <button
              type="button"
              className={controls.primary}
              aria-busy={create.isPending ? true : undefined}
              disabled={create.isPending}
              onClick={() => {
                create.mutate();
              }}
            >
              {create.isPending ? t('wiki.empty.creating') : t('wiki.empty.create')}
            </button>
          ) : undefined
        }
      />
      {create.isError && <ErrorNotice failure={failureOf(create.error)} />}
    </section>
  );
}

const pageUrl = (ctx: Ctx, id: string, tail?: 'edit' | 'history') =>
  `${ctx.base}/${encodeURIComponent(id)}${tail === undefined ? '' : `/${tail}`}`;

function WikiHome({ ctx }: { ctx: Ctx }) {
  const { t } = useTranslation();
  const pages = useWikiPages(ctx.link.wiki_project_id);
  const { hasNextPage, isFetchingNextPage, fetchNextPage } = pages;
  // Every page, so the tree and the next id are whole.
  useEffect(() => {
    if (hasNextPage && !isFetchingNextPage) void fetchNextPage();
  }, [hasNextPage, isFetchingNextPage, fetchNextPage]);
  const actions = ctx.canEdit ? (
    <Link className={controls.primary} to={`${ctx.base}/new`}>
      {t('wiki.newPage')}
    </Link>
  ) : undefined;
  const header = (
    <PageHeader
      title={ctx.link.display_name}
      titleId="wiki-title"
      subtitle={t('wiki.subtitle')}
      actions={actions}
    />
  );
  if (pages.isPending) {
    return (
      <section aria-labelledby="wiki-title">
        {header}
        <Loading />
      </section>
    );
  }
  if (pages.isError) {
    return (
      <section aria-labelledby="wiki-title">
        {header}
        <RepoError
          error={pages.error}
          onRetry={() => {
            void pages.refetch();
          }}
        />
      </section>
    );
  }
  const entries: PageEntry[] = pages.data.pages.flatMap((p) => p.documents);
  const head = pages.data.pages[0]?.ref.commit ?? null;
  return (
    <section aria-labelledby="wiki-title" className={styles.wiki}>
      {header}
      <LintLine ctx={ctx} head={head} entries={entries} />
      {entries.length === 0 ? (
        <EmptyState
          icon="folder"
          reason={t('wiki.noPages')}
          {...(actions === undefined ? {} : { action: actions })}
        />
      ) : (
        <nav aria-label={t('wiki.pages')} className={styles.tree}>
          <Tree ctx={ctx} tree={pageTree(entries)} />
        </nav>
      )}
    </section>
  );
}

function Tree({ ctx, tree }: { ctx: Ctx; tree: PageTree }) {
  return (
    <ul className={styles.treeList}>
      {tree.folders.map((f) => (
        <li key={`d:${f.name}`}>
          <details open className={styles.folder}>
            <summary>{f.name}</summary>
            <Tree ctx={ctx} tree={f.tree} />
          </details>
        </li>
      ))}
      {tree.pages.map((p) => (
        <li key={`p:${p.id}`}>
          <Link to={pageUrl(ctx, p.id)} className={styles.page}>
            {p.title ?? p.id}
          </Link>
        </li>
      ))}
    </ul>
  );
}

/** The one line when the head's lint is red: which page, which line, and
 * the link that fixes it. Nothing while it is green, running or unknown. */
function LintLine({ ctx, head, entries }: { ctx: Ctx; head: string | null; entries: PageEntry[] }) {
  const { t } = useTranslation();
  const checks = useWikiChecks(ctx.link.wiki_project_id, head);
  const run = checks.data?.check_runs.find(
    (r) => r.name === DOC_LINT && r.status === 'completed' && RED.includes(r.conclusion ?? ''),
  );
  if (run === undefined) return null;
  const note =
    run.annotations.find((a) => a.level === 'failure') ?? run.annotations[0] ?? null;
  const page = note === null ? undefined : entries.find((e) => e.path === note.path);
  const fix =
    page !== undefined
      ? pageUrl(ctx, page.id, 'edit')
      : note !== null
        ? blobUrl(ctx.link.wiki_path, ctx.link.default_branch, note.path)
        : null;
  return (
    <FixLine fix={fix}>
      {note === null
        ? t('wiki.lint.red')
        : t('wiki.lint.redAt', {
            page: page?.title ?? page?.id ?? note.path,
            line: note.start_line,
            message: note.message,
          })}
    </FixLine>
  );
}

function FixLine({ fix, children }: { fix: string | null; children: ReactNode }) {
  const { t } = useTranslation();
  return (
    <p className={styles.lint} data-testid="wiki-lint">
      <span>{children}</span>{' '}
      {fix !== null && (
        <Link to={fix} className={styles.fix}>
          {t('wiki.lint.fix')}
        </Link>
      )}
    </p>
  );
}

function PageView({ ctx, docId }: { ctx: Ctx; docId: string }) {
  const { t } = useTranslation();
  const wiki = ctx.link.wiki_project_id;
  const doc = useDocument(wiki, docId);
  useDocumentTitle(doc.data?.title ?? docId);
  if (doc.isPending) return <Loading />;
  if (doc.isError) {
    return (
      <>
        <BackLink ctx={ctx} />
        <PageHeader title={docId} />
        <RepoError
          error={doc.error}
          onRetry={() => {
            void doc.refetch();
          }}
        />
      </>
    );
  }
  const d = doc.data;
  const problem = d.problems[0];
  return (
    <article aria-labelledby="wiki-page-title" className={styles.wiki}>
      <BackLink ctx={ctx} />
      <PageHeader
        title={d.title ?? d.id}
        titleId="wiki-page-title"
        actions={
          <>
            {ctx.canEdit && (
              <Link className={controls.primary} to={pageUrl(ctx, d.id, 'edit')}>
                {t('wiki.edit')}
              </Link>
            )}
            <Link className={controls.button} to={pageUrl(ctx, d.id, 'history')}>
              {t('wiki.history')}
            </Link>
          </>
        }
      />
      {d.problem_count > 0 && problem !== undefined && (
        <FixLine fix={ctx.canEdit ? pageUrl(ctx, d.id, 'edit') : null}>
          {t('wiki.lint.pageAt', { line: problem.line })}
        </FixLine>
      )}
      <PageBody ctx={ctx} d={d} />
    </article>
  );
}

function BackLink({ ctx }: { ctx: Ctx }) {
  const { t } = useTranslation();
  return (
    <p className={styles.back}>
      <Link to={ctx.base}>{t('wiki.backToPages')}</Link>
    </p>
  );
}

function PageBody({ ctx, d }: { ctx: Ctx; d: DocumentBody }) {
  const { t } = useTranslation();
  const words: RefWords = { hidden: t('docs.ref.hiddenShort'), broken: t('docs.ref.brokenShort') };
  const project = ctx.link.wiki_path;
  const refName = d.ref.name;
  const linkTo = (path: string, folder: boolean) => {
    const target = docRefTarget(path);
    if (target !== null) return pageUrl(ctx, target.doc);
    return folder ? treeUrl(project, refName, path) : blobUrl(project, refName, path);
  };
  return (
    <div className={styles.body} lang={d.lang ?? undefined}>
      {d.sections.map((s, i) => (
        <section key={`${s.line}-${i}`} {...(s.heading !== null ? { 'aria-label': s.heading } : {})}>
          {s.heading !== null && <h2>{s.heading}</h2>}
          {s.pieces.map((p, j) => (
            <MarkdownView
              key={`${p.line}-${j}`}
              source={linkRefs(p.block === null ? (p.display ?? '') : p.block.display, d.refs, words)}
              project={project}
              refName={refName}
              rawRef={d.ref.commit}
              dir={parentPath(d.path)}
              headingOffset={1}
              linkTo={linkTo}
            />
          ))}
        </section>
      ))}
    </div>
  );
}

/** A new page: the next free id, at the head the list was read at. */
function NewPageForm({ ctx }: { ctx: Ctx }) {
  const wiki = ctx.link.wiki_project_id;
  const pages = useWikiPages(wiki);
  const { hasNextPage, isFetchingNextPage, fetchNextPage } = pages;
  useEffect(() => {
    if (hasNextPage && !isFetchingNextPage) void fetchNextPage();
  }, [hasNextPage, isFetchingNextPage, fetchNextPage]);
  if (pages.isError) {
    return (
      <RepoError
        error={pages.error}
        onRetry={() => {
          void pages.refetch();
        }}
      />
    );
  }
  if (pages.isPending || pages.hasNextPage) return <Loading />;
  const ids = pages.data.pages.flatMap((p) => p.documents.map((d) => d.id));
  const head = pages.data.pages[0]?.ref.commit ?? '';
  const page: WikiPage = {
    front: {
      id: nextWikiId(ids),
      title: '',
      owner: ownerOf(ctx.link.wiki_path),
      classification: 'internal',
      extra: [],
    },
    body: '',
  };
  return <FormBody ctx={ctx} initial={page} path={null} startSha={head} />;
}

/** An existing page: its file read at the branch head (that commit is the
 * save's `start_sha`). */
function EditPageForm({ ctx, docId }: { ctx: Ctx; docId: string }) {
  const wiki = ctx.link.wiki_project_id;
  const doc = useDocument(wiki, docId);
  const path = doc.data?.path ?? '';
  const blob = useBlob(wiki, `refs/heads/${ctx.link.default_branch}`, path, path !== '');
  const failed = doc.isError ? doc.error : blob.isError ? blob.error : null;
  if (failed !== null) {
    return (
      <RepoError
        error={failed}
        onRetry={() => {
          void doc.refetch();
          void blob.refetch();
        }}
      />
    );
  }
  const file = blob.data;
  if (doc.isPending || file === undefined) return <Loading />;
  const parsed =
    file.binary || file.truncated || file.content === null ? null : parsePage(file.content);
  if (parsed === null) return <NotForForm ctx={ctx} path={file.path} />;
  return <FormBody ctx={ctx} initial={parsed} path={file.path} startSha={file.ref.commit} />;
}

function NotForForm({ ctx, path }: { ctx: Ctx; path: string }) {
  const { t } = useTranslation();
  const editor = `/${encodePath(ctx.link.wiki_path)}/-/edit/${ctx.link.default_branch}/${encodePath(path)}`;
  return (
    <>
      <BackLink ctx={ctx} />
      <PageHeader title={t('wiki.editTitle')} />
      <FixLine fix={editor}>{t('wiki.notForForm')}</FixLine>
    </>
  );
}

function FormBody({
  ctx,
  initial,
  path,
  startSha,
}: {
  ctx: Ctx;
  initial: WikiPage;
  /** The page's file; `null` for a new page. */
  path: string | null;
  startSha: string;
}) {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const wiki = ctx.link.wiki_project_id;
  const save = useSingleFileCommit(wiki);
  const bodyId = useId();
  const [title, setTitle] = useState(initial.front.title);
  const [classification, setClassification] = useState<Classification>(initial.front.classification);
  const [body, setBody] = useState(initial.body);
  const [folder, setFolder] = useState('');
  const [tried, setTried] = useState(false);
  const id = initial.front.id;
  const folderClean = cleanFolder(folder);
  const titleError = tried && title.trim() === '' ? t('wiki.form.titleRequired') : null;
  const folderError = folderClean === null ? t('wiki.form.folderInvalid') : null;
  useDocumentTitle(path === null ? t('wiki.newTitle') : t('wiki.editTitle'));

  const onSubmit = (e: SyntheticEvent) => {
    e.preventDefault();
    setTried(true);
    if (title.trim() === '' || folderClean === null || save.isPending) return;
    const target = path ?? (folderClean === '' ? `${id}.md` : `${folderClean}/${id}.md`);
    const content = composePage({
      front: { ...initial.front, title: title.trim(), classification },
      body,
    });
    save.mutate(
      {
        branch: ctx.link.default_branch,
        path: target,
        content,
        start_sha: startSha,
        message:
          path === null
            ? t('wiki.commit.create', { id, title: title.trim() })
            : t('wiki.commit.update', { id, title: title.trim() }),
      },
      {
        onSuccess: () => {
          void queryClient.invalidateQueries({ queryKey: ['documents', wiki] });
          void queryClient.invalidateQueries({ queryKey: ['document', wiki] });
          void queryClient.invalidateQueries({ queryKey: ['wiki-checks', wiki] });
          void navigate(pageUrl(ctx, id));
        },
      },
    );
  };

  const conflict = commitErrorCode(save.error) === CODE_CONFLICT;
  return (
    <section aria-labelledby="wiki-form-title" className={styles.wiki}>
      <BackLink ctx={ctx} />
      <PageHeader
        title={path === null ? t('wiki.newTitle') : t('wiki.editTitle')}
        titleId="wiki-form-title"
        subtitle={t('wiki.form.subtitle')}
      />
      <form className={styles.form} onSubmit={onSubmit} noValidate>
        <TextField label={t('wiki.form.title')} value={title} onChange={setTitle} required error={titleError} />
        <SelectField
          label={t('wiki.form.classification')}
          value={classification}
          onChange={(v) => {
            if ((CLASSIFICATIONS as readonly string[]).includes(v)) setClassification(v as Classification);
          }}
        >
          <option value="public">{t('docs.class.public')}</option>
          <option value="internal">{t('docs.class.internal')}</option>
          <option value="confidential">{t('docs.class.confidential')}</option>
        </SelectField>
        {path === null && (
          <TextField
            label={t('wiki.form.folder')}
            value={folder}
            onChange={setFolder}
            help={t('wiki.form.folderHelp')}
            error={folderError}
          />
        )}
        <div className={controls.field}>
          <label htmlFor={bodyId} className={controls.label}>
            {t('wiki.form.body')}
          </label>
          <textarea
            id={bodyId}
            className={`${controls.input} ${styles.text}`}
            value={body}
            rows={16}
            aria-describedby={`${bodyId}-help`}
            onChange={(e) => {
              setBody(e.target.value);
            }}
          />
          <p id={`${bodyId}-help`} className={controls.help}>
            {t('wiki.form.bodyHelp')}
          </p>
        </div>
        {save.isError && (
          <div className={styles.saveError}>
            <ErrorNotice failure={failureOf(save.error)} />
            {conflict && <p>{t('wiki.form.conflict')}</p>}
          </div>
        )}
        <div className={styles.formActions}>
          <button
            type="submit"
            className={controls.primary}
            aria-busy={save.isPending ? true : undefined}
            disabled={save.isPending}
          >
            {save.isPending ? t('wiki.form.saving') : t('wiki.form.save')}
          </button>
          <Link to={path === null ? ctx.base : pageUrl(ctx, id)} className={controls.button}>
            {t('wiki.form.cancel')}
          </Link>
        </div>
      </form>
    </section>
  );
}

function PageHistory({ ctx, docId }: { ctx: Ctx; docId: string }) {
  const { t, i18n } = useTranslation();
  const api = useApi();
  const wiki = ctx.link.wiki_project_id;
  const doc = useDocument(wiki, docId);
  const path = doc.data?.path ?? '';
  const commits = useInfiniteQuery({
    ...commitsQuery(api, wiki, `refs/heads/${ctx.link.default_branch}`, path),
    enabled: path !== '',
  });
  useDocumentTitle(t('wiki.historyTitle', { page: doc.data?.title ?? docId }));
  const failed = doc.isError ? doc.error : commits.isError ? commits.error : null;
  const header = (
    <>
      <BackLink ctx={ctx} />
      <PageHeader
        title={t('wiki.historyTitle', { page: doc.data?.title ?? docId })}
        titleId="wiki-history-title"
      />
    </>
  );
  if (failed !== null) {
    return (
      <>
        {header}
        <RepoError
          error={failed}
          onRetry={() => {
            void doc.refetch();
            void commits.refetch();
          }}
        />
      </>
    );
  }
  if (doc.isPending || commits.data === undefined) return <Loading />;
  const list = commits.data.pages.flatMap((p) => p.commits);
  return (
    <section aria-labelledby="wiki-history-title" className={styles.wiki}>
      {header}
      <ol className={styles.history}>
        {list.map((c) => (
          <li key={c.oid}>
            <Link to={commitUrl(ctx.link.wiki_path, c.oid)}>{c.subject}</Link>
            <span className={styles.meta}>
              <span>{c.author_name}</span> {'·'} {formatDateTime(c.authored_at, i18n.language)}
            </span>
          </li>
        ))}
      </ol>
      {commits.hasNextPage && (
        <button
          type="button"
          className={controls.button}
          disabled={commits.isFetchingNextPage}
          onClick={() => {
            void commits.fetchNextPage();
          }}
        >
          {t('wiki.more')}
        </button>
      )}
    </section>
  );
}
