// /-/notes (plan 20 N2, ADR-0040 §6): my notes, from the "My work" sidebar.
//
// - No notes yet: one line and one button ("Start my notes": the private
//   project <me>/notes is created in one step).
// - Notes moved or deleted (FG-KNOW-0005): one line and "Create them again".
// - Notes: the quick note (saved with one click or Ctrl+Enter as one commit),
//   then my notes newest first, and a search over them only.
//
// The search finds a note a little after it is saved (the index follows the
// push); the list reads the repository, so a saved note is there at once.
import type { TFunction } from 'i18next';
import { useId, useRef, useState, type KeyboardEvent, type RefObject, type SyntheticEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { Link } from 'react-router';

import { failureOf } from '../../api/client';
import { MAX_CONTENT_BYTES } from '../../api/fileCommits';
import {
  NoteExists,
  useCreateNotes,
  useNoteList,
  useNoteSearch,
  useNotes,
  useQuickNote,
  type NoteHit,
  type NoteSummary,
  type Notes,
  type SavedNote,
} from '../../api/notes';
import { MIN_QUERY_CHARS, queryChars, snippetPieces } from '../../api/search';
import controls from '../../components/controls.module.css';
import { ErrorNotice } from '../../components/ErrorNotice';
import { QueryError } from '../../components/QueryError';
import { SkeletonList } from '../../components/Skeleton';
import { formatDateTime, formatRelative } from '../../features/labels';
import { noteIdOfPath } from '../../features/noteText';
import { blobUrl, documentUrl, encodePath } from '../../features/repoUrls';
import { useDocumentTitle } from '../../hooks/useDocumentTitle';
import { EmptyState } from '../../ui/EmptyState';
import { PageHeader } from '../../ui/PageHeader';
import styles from './Notes.module.css';

/** `N-20261126-093000` → `2026-11-26T09:30:00Z` (the moment it was written). */
export function noteTime(id: string): string | null {
  const m = /^N-(\d{4})(\d{2})(\d{2})-(\d{2})(\d{2})(\d{2})$/.exec(id);
  if (m === null) return null;
  return `${m[1] ?? ''}-${m[2] ?? ''}-${m[3] ?? ''}T${m[4] ?? ''}:${m[5] ?? ''}:${m[6] ?? ''}Z`;
}

function When({ iso }: { iso: string | null }) {
  const { i18n } = useTranslation();
  if (iso === null) return null;
  return (
    <time dateTime={iso} title={formatDateTime(iso, i18n.language)}>
      {formatRelative(iso, i18n.language)}
    </time>
  );
}

/** A note's page: the document view by its id (a note is a document). */
function noteUrl(notes: Notes, path: string, id: string | null): string {
  return id === null ? blobUrl(notes.path, notes.default_branch, path) : documentUrl(notes.path, id);
}

function bytesOf(text: string): number {
  return new TextEncoder().encode(text).length;
}

function saveError(t: TFunction, error: unknown) {
  if (error instanceof NoteExists) {
    return (
      <p role="alert" className={styles.inlineError}>
        {t('notes.quick.sameSecond')}
      </p>
    );
  }
  return <ErrorNotice failure={failureOf(error)} />;
}

function QuickNote({ notes, bodyRef }: { notes: Notes; bodyRef: RefObject<HTMLTextAreaElement | null> }) {
  const { t } = useTranslation();
  const save = useQuickNote(notes);
  const titleId = useId();
  const bodyId = useId();
  const [title, setTitle] = useState('');
  const [body, setBody] = useState('');
  const [problem, setProblem] = useState<string | null>(null);
  const [saved, setSaved] = useState<SavedNote | null>(null);
  const formRef = useRef<HTMLFormElement>(null);

  const onSubmit = (e: SyntheticEvent) => {
    e.preventDefault();
    if (save.isPending) return;
    if (body.trim() === '') {
      setProblem(t('notes.quick.bodyRequired'));
      bodyRef.current?.focus();
      return;
    }
    if (bytesOf(body) > MAX_CONTENT_BYTES - 1024) {
      setProblem(t('notes.quick.tooLong', { kib: (MAX_CONTENT_BYTES - 1024) / 1024 }));
      bodyRef.current?.focus();
      return;
    }
    setProblem(null);
    setSaved(null);
    save.mutate(
      { title, body },
      {
        onSuccess: (note) => {
          setSaved(note);
          setTitle('');
          setBody('');
          bodyRef.current?.focus();
        },
      },
    );
  };

  const onKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === 'Enter' && (e.ctrlKey || e.metaKey) && !e.nativeEvent.isComposing) {
      e.preventDefault();
      formRef.current?.requestSubmit();
    }
  };

  return (
    <section aria-labelledby={`${titleId}-h`} className={styles.quick}>
      <h2 id={`${titleId}-h`} className={styles.sectionTitle}>
        {t('notes.quick.title')}
      </h2>
      <form ref={formRef} onSubmit={onSubmit} noValidate>
        <div className={controls.field}>
          <label htmlFor={titleId} className={controls.label}>
            {t('notes.quick.titleLabel')}
          </label>
          <input
            id={titleId}
            className={controls.input}
            value={title}
            aria-describedby={`${titleId}-help`}
            onChange={(e) => {
              setTitle(e.target.value);
            }}
          />
          <p id={`${titleId}-help`} className={controls.help}>
            {t('notes.quick.titleHelp')}
          </p>
        </div>
        <div className={controls.field}>
          <label htmlFor={bodyId} className={controls.label}>
            {t('notes.quick.bodyLabel')}
          </label>
          <textarea
            id={bodyId}
            ref={bodyRef}
            className={`${controls.input} ${styles.body}`}
            value={body}
            rows={6}
            required
            aria-invalid={problem !== null ? true : undefined}
            aria-describedby={problem !== null ? `${bodyId}-help ${bodyId}-error` : `${bodyId}-help`}
            aria-keyshortcuts="Control+Enter Meta+Enter"
            onKeyDown={onKeyDown}
            onChange={(e) => {
              setBody(e.target.value);
              if (problem !== null) setProblem(null);
            }}
          />
          <p id={`${bodyId}-help`} className={controls.help}>
            {t('notes.quick.bodyHelp')}
          </p>
          {problem !== null && (
            <p id={`${bodyId}-error`} className={controls.fieldError}>
              {problem}
            </p>
          )}
        </div>
        {save.isError && <div className={styles.saveError}>{saveError(t, save.error)}</div>}
        <div className={styles.actions}>
          <button
            type="submit"
            className={controls.primary}
            aria-busy={save.isPending ? true : undefined}
            aria-disabled={save.isPending ? 'true' : undefined}
          >
            {save.isPending ? t('notes.quick.saving') : t('notes.quick.save')}
          </button>
          <span className={styles.shortcut} aria-hidden="true">
            {t('notes.quick.shortcut')}
          </span>
        </div>
        <p role="status" className={styles.saved}>
          {saved !== null && (
            <>
              <span>{t('notes.quick.saved', { title: saved.title })}</span>{' '}
              <Link to={documentUrl(notes.path, saved.id)}>{t('notes.quick.open')}</Link>
            </>
          )}
        </p>
      </form>
    </section>
  );
}

function NoteItem({ notes, note }: { notes: Notes; note: NoteSummary }) {
  const { t } = useTranslation();
  return (
    <li className={styles.item}>
      <Link to={noteUrl(notes, note.path, note.id)} className={styles.itemTitle}>
        {note.title ?? note.id}
      </Link>
      <p className={styles.itemMeta}>
        <When iso={noteTime(note.id)} />
        {note.classification !== 'internal' && (
          <span className={styles.badge}>
            {note.classification === 'confidential' ? t('docs.class.confidential') : t('docs.class.public')}
          </span>
        )}
      </p>
    </li>
  );
}

function NoteList({ notes, onWrite }: { notes: Notes; onWrite: () => void }) {
  const { t } = useTranslation();
  const list = useNoteList(notes.project_id);
  if (list.isPending) return <SkeletonList />;
  if (list.isError) {
    return (
      <QueryError
        error={list.error}
        onRetry={() => {
          void list.refetch();
        }}
      />
    );
  }
  const { notes: items, capped, incomplete } = list.data;
  if (items.length === 0) {
    return (
      <EmptyState
        icon="note"
        compact
        reason={t('notes.list.empty')}
        action={
          <button type="button" className={controls.button} onClick={onWrite}>
            {t('notes.list.write')}
          </button>
        }
      />
    );
  }
  return (
    <>
      <ol className={styles.list}>
        {items.map((note) => (
          <NoteItem key={note.path} notes={notes} note={note} />
        ))}
      </ol>
      {(capped || incomplete) && <p className={styles.note}>{t('notes.list.capped')}</p>}
    </>
  );
}

function HitItem({ notes, hit }: { notes: Notes; hit: NoteHit }) {
  const path = hit.path ?? '';
  const id = noteIdOfPath(path);
  const { snippet } = hit;
  return (
    <li className={styles.item}>
      <Link to={noteUrl(notes, path, id)} className={styles.itemTitle}>
        {hit.title}
      </Link>
      <p className={styles.itemMeta}>
        <When iso={id === null ? hit.updated_at : noteTime(id)} />
      </p>
      {snippet.text !== '' && (
        <p
          className={[styles.snippet, snippet.cut_before ? styles.cutBefore : '', snippet.cut_after ? styles.cutAfter : '']
            .filter(Boolean)
            .join(' ')}
        >
          {snippetPieces(snippet).map((p, i) =>
            p.mark ? (
              <mark key={i} className={styles.mark}>
                {p.text}
              </mark>
            ) : (
              <span key={i}>{p.text}</span>
            ),
          )}
        </p>
      )}
    </li>
  );
}

function SearchResults({ notes, q, onClear }: { notes: Notes; q: string; onClear: () => void }) {
  const { t } = useTranslation();
  const search = useNoteSearch(notes.project_id, q, true);
  const items = search.data?.pages.flatMap((p) => p.items.filter((h) => h.project_id === notes.project_id)) ?? [];
  return (
    <>
      {search.isPending && <SkeletonList />}
      {search.isSuccess && items.length > 0 && (
        <p role="status" className={styles.status}>
          {t('notes.search.shown', { count: items.length })}
        </p>
      )}
      {items.length > 0 && (
        <ol className={styles.list}>
          {items.map((hit, i) => (
            <HitItem key={`${hit.path ?? hit.title}#${String(i)}`} notes={notes} hit={hit} />
          ))}
        </ol>
      )}
      {search.isError && (
        <QueryError
          error={search.error}
          onRetry={() => {
            void (search.isFetchNextPageError ? search.fetchNextPage() : search.refetch());
          }}
        />
      )}
      {search.isSuccess && items.length === 0 && !search.hasNextPage && (
        <EmptyState
          icon="search"
          compact
          live
          reason={t('notes.search.none', { q })}
          hints={[t('notes.search.lag')]}
          action={
            <button type="button" className={controls.button} onClick={onClear}>
              {t('notes.search.backToList')}
            </button>
          }
        />
      )}
      {search.isSuccess && search.hasNextPage && (
        <div className={styles.more}>
          <button
            type="button"
            className={controls.button}
            aria-disabled={search.isFetchingNextPage ? 'true' : undefined}
            onClick={() => {
              if (!search.isFetchingNextPage) void search.fetchNextPage();
            }}
          >
            {search.isFetchingNextPage ? t('common.loading') : t('notes.search.more')}
          </button>
        </div>
      )}
    </>
  );
}

function MyNotes({ notes, onWrite }: { notes: Notes; onWrite: () => void }) {
  const { t } = useTranslation();
  const headId = useId();
  const [text, setText] = useState('');
  const [q, setQ] = useState('');
  const long = queryChars(q) >= MIN_QUERY_CHARS;
  const clear = () => {
    setText('');
    setQ('');
  };
  return (
    <section aria-labelledby={headId} className={styles.mine}>
      <h2 id={headId} className={styles.sectionTitle}>
        {q === '' ? t('notes.list.title') : t('notes.search.title')}
      </h2>
      <form
        role="search"
        className={styles.searchForm}
        onSubmit={(e) => {
          e.preventDefault();
          setQ(text.trim());
        }}
      >
        <input
          type="search"
          className={`${controls.input} ${styles.searchInput}`}
          aria-label={t('notes.search.label')}
          value={text}
          onChange={(e) => {
            setText(e.target.value);
            if (e.target.value.trim() === '') setQ('');
          }}
        />
        <button type="submit" className={controls.button}>
          {t('notes.search.submit')}
        </button>
        {(q !== '' || text !== '') && (
          <button type="button" className={controls.button} onClick={clear}>
            {t('notes.search.clear')}
          </button>
        )}
      </form>
      {q === '' && <NoteList notes={notes} onWrite={onWrite} />}
      {q !== '' && !long && (
        <p role="status" className={styles.status}>
          {t('notes.search.tooShort', { n: MIN_QUERY_CHARS })}
        </p>
      )}
      {long && <SearchResults key={q} notes={notes} q={q} onClear={clear} />}
    </section>
  );
}

function Start({ missing }: { missing: { requestId: string } | null }) {
  const { t } = useTranslation();
  const create = useCreateNotes();
  return (
    <>
      <EmptyState
        icon="note"
        reason={missing === null ? t('notes.start.reason') : t('notes.start.missing')}
        hints={
          missing === null
            ? [t('notes.start.hint')]
            : [t('notes.start.missingHint'), t('notes.start.requestId', { id: missing.requestId })]
        }
        action={
          <button
            type="button"
            className={controls.primary}
            aria-busy={create.isPending ? true : undefined}
            aria-disabled={create.isPending ? 'true' : undefined}
            onClick={() => {
              if (!create.isPending) create.mutate();
            }}
          >
            {create.isPending
              ? t('notes.start.creating')
              : missing === null
                ? t('notes.start.create')
                : t('notes.start.recreate')}
          </button>
        }
      />
      {create.isError && (
        <div className={styles.saveError}>
          <ErrorNotice failure={failureOf(create.error)} />
        </div>
      )}
    </>
  );
}

export function NotesScreen() {
  const { t } = useTranslation();
  const notes = useNotes();
  const bodyRef = useRef<HTMLTextAreaElement>(null);
  useDocumentTitle(t('notes.title'));
  const live = notes.data?.kind === 'live' ? notes.data.notes : null;
  return (
    <div className={styles.page}>
      <PageHeader
        title={t('notes.title')}
        subtitle={t('notes.subtitle')}
        actions={
          live !== null ? (
            <Link to={`/${encodePath(live.path)}`} className={controls.button}>
              {t('notes.openProject')}
            </Link>
          ) : undefined
        }
      />
      {notes.isPending && <SkeletonList />}
      {notes.isError && (
        <QueryError
          error={notes.error}
          onRetry={() => {
            void notes.refetch();
          }}
        />
      )}
      {notes.data?.kind === 'none' && <Start missing={null} />}
      {notes.data?.kind === 'missing' && <Start missing={{ requestId: notes.data.requestId }} />}
      {live !== null && (
        <div className={styles.layout}>
          <QuickNote notes={live} bodyRef={bodyRef} />
          <MyNotes
            notes={live}
            onWrite={() => {
              bodyRef.current?.focus();
            }}
          />
        </div>
      )}
    </div>
  );
}
