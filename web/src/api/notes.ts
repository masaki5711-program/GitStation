// The caller's personal notes (plan 20 N1/N2, ADR-0040 §6): the private
// project `<user>/notes`.
//
// - `GET /api/v1/user/notes` names it. 404 is "no notes yet"
//   (FG-REPO-0005) or "the notes project was moved or deleted"
//   (FG-KNOW-0005, create them again); the screen words the two apart by
//   the code, which is never shown (CLAUDE.md contract 3).
// - `POST /api/v1/user/notes` creates them (or replaces a stale pointer).
// - A quick note is ONE direct commit of `N-YYYYMMDD-HHMMSS.md` on `main`
//   through the simple editor's commit, as `gstn note new` does: resolve the
//   branch head, make sure no note of that second exists there (the commit
//   would replace it), then commit.
// - The list is the document list of the notes project (`doctype=note`);
//   the search is the knowledge search on that project (`scope=document`).
import { useInfiniteQuery, useMutation, useQuery, useQueryClient } from '@tanstack/react-query';

import { ApiFailure, call, isRecord } from './client';
import { useApi } from './context';
import { CODE_CONFLICT, CommitFailure } from './fileCommits';
import type { components } from './schema.gen';
import { NOTE_DOCTYPE, defaultTitle, noteId, notePath, quickNote } from '../features/noteText';

type S = components['schemas'];
export type Notes = S['NotesBody'];
export type NoteSummary = S['DocumentSummaryBody'];
export type NoteHit = S['SearchHit'];

/** No notes yet. */
export const CODE_NO_NOTES = 'FG-REPO-0005';
/** The notes project was moved out of `<user>/notes` or is pending deletion. */
export const CODE_NOTES_MISSING = 'FG-KNOW-0005';

/** Notes asked for at a time, and the most the list reads (newest first
 * needs every page: the API sorts by id, oldest first). */
export const NOTES_PAGE = 100;
export const NOTES_MAX_PAGES = 20;
/** Hits asked for at a time (the API allows 1-50). */
export const NOTE_HITS_PAGE = 20;

/** The answer of `GET /api/v1/user/notes`, as the screen tells it apart. */
export type NotesState =
  | { kind: 'live'; notes: Notes }
  | { kind: 'none' }
  | { kind: 'missing'; requestId: string };

/** A failure of a notes call with the server's code (for choosing words only). */
function errorCode(error: unknown): string | null {
  return isRecord(error) && typeof error.error_code === 'string' ? error.error_code : null;
}

export const notesKeys = {
  pointer: ['user-notes'] as const,
  list: (project: number) => ['documents', project, 'notes'] as const,
  search: (project: number, q: string) => ['notes-search', project, q] as const,
};

export function useNotes() {
  const api = useApi();
  return useQuery({
    queryKey: notesKeys.pointer,
    queryFn: async ({ signal }): Promise<NotesState> => {
      // Set inside the call (a holder: the closure's write is not seen by flow analysis).
      const seen: { code: string | null } = { code: null };
      try {
        const notes = await call(async () => {
          const result = await api.GET('/api/v1/user/notes', { signal });
          seen.code = errorCode(result.error);
          return result;
        });
        return { kind: 'live', notes };
      } catch (e) {
        if (e instanceof ApiFailure && e.failure.kind === 'api' && e.failure.status === 404) {
          if (seen.code === CODE_NOTES_MISSING) return { kind: 'missing', requestId: e.failure.requestId };
          if (seen.code === CODE_NO_NOTES) return { kind: 'none' };
        }
        throw e;
      }
    },
  });
}

export function useCreateNotes() {
  const api = useApi();
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: () => call(() => api.POST('/api/v1/user/notes', {})),
    onSuccess: async () => {
      await queryClient.invalidateQueries({ queryKey: notesKeys.pointer });
    },
  });
}

/** Every note of the notes' default branch, newest first. `capped`: more
 * than [`NOTES_MAX_PAGES`] pages exist, so the newest ones may be missing
 * (the search still finds them). */
export function useNoteList(projectId: number) {
  const api = useApi();
  return useQuery({
    queryKey: notesKeys.list(projectId),
    queryFn: async ({ signal }) => {
      const notes: NoteSummary[] = [];
      let after: string | undefined;
      for (let page = 0; page < NOTES_MAX_PAGES; page += 1) {
        const body = await call(() =>
          api.GET('/api/v1/projects/{id}/documents', {
            signal,
            params: {
              path: { id: projectId },
              query: { doctype: NOTE_DOCTYPE, limit: NOTES_PAGE, ...(after === undefined ? {} : { after }) },
            },
          }),
        );
        notes.push(...body.documents);
        if (body.next_after === null) {
          return { notes: notes.reverse(), capped: false, incomplete: body.incomplete !== null };
        }
        after = body.next_after;
      }
      return { notes: notes.reverse(), capped: true, incomplete: false };
    },
  });
}

/** The knowledge search on the notes project only. */
export function useNoteSearch(projectId: number, q: string, enabled: boolean) {
  const api = useApi();
  return useInfiniteQuery({
    queryKey: notesKeys.search(projectId, q),
    initialPageParam: undefined as string | undefined,
    queryFn: ({ signal, pageParam }) =>
      call(() =>
        api.GET('/api/v1/search', {
          signal,
          params: {
            query: {
              q,
              scope: 'document',
              project: projectId,
              limit: NOTE_HITS_PAGE,
              ...(pageParam === undefined ? {} : { cursor: pageParam }),
            },
          },
        }),
      ),
    getNextPageParam: (last) => (last.has_more ? (last.next_cursor ?? undefined) : undefined),
    enabled,
  });
}

export interface QuickNoteInput {
  title: string;
  body: string;
}

export interface SavedNote {
  id: string;
  title: string;
  commit: string;
}

/** The note of this second already exists at the head (one note per second). */
export class NoteExists extends Error {
  constructor() {
    super('note_exists');
    this.name = 'NoteExists';
  }
}

/** Save one quick note; `now` is the clock (tests pass their own). */
export function useQuickNote(notes: Notes, now: () => Date = () => new Date()) {
  const api = useApi();
  const queryClient = useQueryClient();
  const project = notes.project_id;
  return useMutation({
    mutationFn: async (input: QuickNoteInput): Promise<SavedNote> => {
      const id = noteId(now());
      if (id === null) throw new ApiFailure({ kind: 'unexpected', status: null, requestId: null });
      // As `gstn note new --title`: a title given is written as typed.
      const title = input.title.trim() === '' ? defaultTitle(input.body, id) : input.title;
      const path = notePath(id);
      const resolved = await call(() =>
        api.GET('/api/v1/projects/{id}/repository/resolve', {
          params: { path: { id: project }, query: { spec: notes.default_branch } },
        }),
      );
      const start = resolved.ref.commit;
      // Never replace a note: the commit replaces a file that exists at
      // `start` (a second note in the same second).
      try {
        await call(() =>
          api.GET('/api/v1/projects/{id}/repository/blob', {
            params: { path: { id: project }, query: { ref: start, path } },
          }),
        );
        throw new NoteExists();
      } catch (e) {
        if (!(e instanceof ApiFailure && e.failure.kind === 'api' && e.failure.status === 404)) throw e;
      }
      const seen: { code: string | null } = { code: null };
      try {
        const done = await call(async () => {
          const result = await api.POST('/api/v1/projects/{id}/repository/commits/single-file', {
            params: { path: { id: project } },
            body: {
              branch: notes.default_branch,
              start_sha: start,
              path,
              content: quickNote(id, title, notes.owner, input.body),
              message: `Add note ${id}`,
            },
          });
          seen.code = errorCode(result.error);
          return result;
        });
        return { id, title, commit: done.commit_sha };
      } catch (e) {
        // A note of this second written after `start` (another tab): the
        // same "save again" as one found at the head.
        if (seen.code === CODE_CONFLICT) throw new NoteExists();
        if (e instanceof ApiFailure) throw new CommitFailure(e, seen.code);
        throw e;
      }
    },
    onSuccess: async () => {
      for (const word of ['refs', 'resolve-spec', 'tree', 'blob', 'commits', 'commits-latest']) {
        void queryClient.invalidateQueries({ queryKey: [word, project], refetchType: 'none' });
      }
      void queryClient.invalidateQueries({ queryKey: ['document', project], refetchType: 'none' });
      await queryClient.invalidateQueries({ queryKey: notesKeys.list(project) });
    },
  });
}
