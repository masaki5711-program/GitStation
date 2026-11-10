// One document in the plan 13 format (`/-/documents/{id}[?ref=&lang=]`,
// plan 13 stage 3): the front matter as a summary, the sections with their
// headings, the text drawn by the Markdown renderer (the server already
// joined the one-sentence-per-line prose: `display`), blocks grouped by kind
// as tables (id, attributes, text; deprecated rows muted and said so), and
// the references. A reference to a more confidential document is its id
// only (Q14): the server never sends its title. Below them, the documents
// that refer to this one (`ReferencedBy`).
import { Fragment, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { Link, useSearchParams } from 'react-router';

import { isDocId, useDocument } from '../../api/documents';
import type { DocumentBlock, DocumentBody, DocumentPiece, DocumentRef } from '../../api/types';
import { Loading } from '../../components/Loading';
import { docRefTarget, linkRefs, type RefWords } from '../../features/docRefs';
import { MarkdownView } from '../../features/MarkdownView';
import {
  blobUrl,
  blockAnchor,
  documentUrl,
  documentsUrl,
  parentPath,
  treeUrl,
} from '../../features/repoUrls';
import { useDocumentTitle } from '../../hooks/useDocumentTitle';
import { NotFound } from '../NotFound';
import { RepoError } from './common';
import { ReferencedBy } from './ReferencedBy';
import styles from './Documents.module.css';

export function ClassificationBadge({ value }: { value: string }) {
  const { t } = useTranslation();
  const known = value === 'public' || value === 'internal' || value === 'confidential';
  const cls =
    value === 'confidential'
      ? styles.confidential
      : value === 'public'
        ? styles.public
        : styles.internal;
  return (
    <span className={cls} data-testid="classification">
      {value === 'confidential'
        ? t('docs.class.confidential')
        : value === 'public'
          ? t('docs.class.public')
          : known
            ? t('docs.class.internal')
            : value}
    </span>
  );
}

export function DocumentScreen({
  projectId,
  project,
  docId,
}: {
  projectId: number;
  project: string;
  docId: string;
}) {
  const [params] = useSearchParams();
  const ref = params.get('ref') ?? undefined;
  const lang = params.get('lang') ?? undefined;
  if (!isDocId(docId)) return <NotFound />;
  return <Document projectId={projectId} project={project} docId={docId} ref_={ref} lang={lang} />;
}

function Document({
  projectId,
  project,
  docId,
  ref_,
  lang,
}: {
  projectId: number;
  project: string;
  docId: string;
  ref_: string | undefined;
  lang: string | undefined;
}) {
  const { t } = useTranslation();
  const doc = useDocument(projectId, docId, ref_, lang);
  useDocumentTitle(doc.data?.title ?? docId);
  if (doc.isPending) return <Loading />;
  if (doc.isError) {
    return (
      <>
        <h1 className={styles.title}>{docId}</h1>
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
  const words: RefWords = { hidden: t('docs.ref.hiddenShort'), broken: t('docs.ref.brokenShort') };
  const refName = d.ref.name;
  const dir = parentPath(d.path);
  const keep = ref_ === undefined ? {} : { ref: ref_ };
  const linkTo = (path: string, folder: boolean) => {
    const target = docRefTarget(path);
    if (target !== null) return documentUrl(project, target.doc, { ...keep, ...target });
    return folder ? treeUrl(project, refName, path) : blobUrl(project, refName, path);
  };
  const markdown = (source: string) => (
    <MarkdownView
      source={linkRefs(source, d.refs, words)}
      project={project}
      refName={refName}
      rawRef={d.ref.commit}
      dir={dir}
      headingOffset={1}
      linkTo={linkTo}
    />
  );

  return (
    <article aria-labelledby="doc-title" className={styles.document}>
      <p className={styles.back}>
        <Link to={documentsUrl(project, keep)}>{t('docs.backToList')}</Link>
      </p>
      <h1 id="doc-title" className={styles.title}>
        {d.title ?? d.id}
      </h1>
      <Summary d={d} project={project} />
      {d.translations.length > 0 && (
        <nav aria-label={t('docs.translations')} className={styles.translations}>
          <span>{t('docs.translations')}:</span>
          <ul>
            <li>
              {d.lang === null ? (
                <span aria-current="page">{t('docs.original')}</span>
              ) : (
                <Link to={documentUrl(project, d.id, keep)}>{t('docs.original')}</Link>
              )}
            </li>
            {d.translations.map((l) => (
              <li key={l}>
                {d.lang === l ? (
                  <span aria-current="page" lang={l}>
                    {l}
                  </span>
                ) : (
                  <Link to={documentUrl(project, d.id, { ...keep, lang: l })} lang={l}>
                    {l}
                  </Link>
                )}
              </li>
            ))}
          </ul>
        </nav>
      )}
      {d.incomplete !== null && (
        <p className={styles.warning} role="status" data-testid="doc-incomplete">
          {t('docs.incomplete', { code: d.incomplete.error_code })}
        </p>
      )}
      {d.problem_count > 0 && (
        <p className={styles.warning} role="status" data-testid="doc-problems">
          {t('docs.problems', { count: d.problem_count })}
        </p>
      )}
      <div lang={d.lang ?? undefined}>
        {d.sections.map((s, i) => (
          <section
            key={`${s.line}-${i}`}
            className={styles.section}
            {...(s.heading !== null ? { 'aria-label': s.heading } : {})}
          >
            {s.heading !== null && <h2>{s.heading}</h2>}
            {groups(s.pieces).map((g, j) =>
              g.type === 'text' ? (
                <Fragment key={j}>{markdown(g.text)}</Fragment>
              ) : (
                <BlockGroup key={j} kind={g.kind} blocks={g.blocks} render={markdown} />
              ),
            )}
          </section>
        ))}
      </div>
      <References refs={d.refs} project={project} keep={keep} />
      <ReferencedBy
        key={`${d.id}@${ref_ ?? ''}`}
        projectId={projectId} project={project} docId={d.id} keep={keep} />
    </article>
  );
}

function Summary({ d, project }: { d: DocumentBody; project: string }) {
  const { t } = useTranslation();
  return (
    <dl className={styles.summary}>
      <div>
        <dt>{t('docs.id')}</dt>
        <dd>
          <code>{d.id}</code>
        </dd>
      </div>
      <div>
        <dt>{t('docs.doctype')}</dt>
        <dd>
          <code>{d.doctype}</code>
        </dd>
      </div>
      <div>
        <dt>{t('docs.owner')}</dt>
        <dd>{d.owner ?? t('docs.none')}</dd>
      </div>
      <div>
        <dt>{t('docs.classification')}</dt>
        <dd>
          <ClassificationBadge value={d.classification} />
        </dd>
      </div>
      <div>
        <dt>{t('docs.revision')}</dt>
        <dd>
          <code>{d.ref.name}</code>
        </dd>
      </div>
      <div>
        <dt>{t('docs.file')}</dt>
        <dd>
          <Link to={blobUrl(project, d.ref.name, d.path)}>{d.path}</Link>
        </dd>
      </div>
    </dl>
  );
}

type Group = { type: 'text'; text: string } | { type: 'blocks'; kind: string; blocks: DocumentBlock[] };

/** Consecutive blocks of one kind (with ids) become one table; text stays
 * text; a plain container stays on its own. */
function groups(pieces: DocumentPiece[]): Group[] {
  const out: Group[] = [];
  for (const p of pieces) {
    if (p.block === null) {
      out.push({ type: 'text', text: p.display ?? '' });
      continue;
    }
    const last = out[out.length - 1];
    if (
      last !== undefined &&
      last.type === 'blocks' &&
      last.kind === p.block.kind &&
      p.block.id !== null &&
      last.blocks[0]?.id !== null
    ) {
      last.blocks.push(p.block);
    } else {
      out.push({ type: 'blocks', kind: p.block.kind, blocks: [p.block] });
    }
  }
  return out;
}

function BlockGroup({
  kind,
  blocks,
  render,
}: {
  kind: string;
  blocks: DocumentBlock[];
  render: (source: string) => ReactNode;
}) {
  const { t } = useTranslation();
  // A plain container (`::: note`): an aside with its text.
  const first = blocks[0];
  if (first !== undefined && first.id === null) {
    return (
      <aside className={styles.note} aria-label={kind}>
        <p className={styles.noteKind}>{kind}</p>
        {render(first.display)}
      </aside>
    );
  }
  const keys: string[] = [];
  for (const b of blocks) {
    for (const a of b.attrs) if (!keys.includes(a.key) && a.key !== 'status') keys.push(a.key);
  }
  return (
    <div className={styles.scroll} role="region" aria-label={t('docs.blocksOf', { kind })} tabIndex={0}>
      <table className={styles.blocks}>
        <caption>{t('docs.blocksOf', { kind })}</caption>
        <thead>
          <tr>
            <th scope="col">{t('docs.blockId')}</th>
            {keys.map((k) => (
              <th scope="col" key={k}>
                {k}
              </th>
            ))}
            <th scope="col">{t('docs.blockText')}</th>
          </tr>
        </thead>
        <tbody>
          {blocks.map((b) => (
            <tr
              key={b.id ?? ''}
              id={b.id === null ? undefined : blockAnchor(b.id)}
              className={b.deprecated ? styles.deprecated : undefined}
            >
              <th scope="row">
                <code>{b.id}</code>
                {b.deprecated && <span className={styles.deprecatedLabel}>{t('docs.deprecated')}</span>}
              </th>
              {keys.map((k) => (
                <td key={k}>{b.attrs.find((a) => a.key === k)?.value ?? ''}</td>
              ))}
              <td>{render(b.display)}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function References({
  refs,
  project,
  keep,
}: {
  refs: DocumentRef[];
  project: string;
  keep: { ref?: string };
}) {
  const { t } = useTranslation();
  if (refs.length === 0) return null;
  return (
    <section aria-labelledby="doc-refs" className={styles.section}>
      <h2 id="doc-refs">{t('docs.references')}</h2>
      <ul className={styles.refs}>
        {refs.map((r, i) => {
          const label = r.block === null ? r.doc : `${r.doc}#${r.block}`;
          let body;
          switch (r.target) {
            case 'found':
              body = (
                <Link
                  to={documentUrl(project, r.doc, { ...keep, ...(r.block === null ? {} : { block: r.block }) })}
                >
                  {label} {r.title}
                </Link>
              );
              break;
            case 'hidden':
              body = (
                <>
                  <code>{label}</code> <span className={styles.refNote}>{t('docs.ref.hidden')}</span>
                </>
              );
              break;
            case 'external':
              body = (
                <>
                  <code>
                    {r.repo}:{label}
                  </code>{' '}
                  <span className={styles.refNote}>{t('docs.ref.external')}</span>
                </>
              );
              break;
            default:
              body = (
                <>
                  <code>{label}</code> <strong className={styles.broken}>{t('docs.ref.broken')}</strong>
                </>
              );
          }
          return (
            <li key={`${r.line}-${i}`} data-target={r.target}>
              <span className={styles.refLine}>{t('docs.ref.line', { line: r.line })}</span> {body}
            </li>
          );
        })}
      </ul>
    </section>
  );
}
