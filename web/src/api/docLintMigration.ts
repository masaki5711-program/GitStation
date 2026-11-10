// A project's document lint migration (plan 13 Q13): broken document
// references are errors in forge/doc-lint; a Maintainer may lower them to
// warnings until a date. The dates are `YYYY-MM-DD` strings, compared by the
// server with its database's date (`today`, `max_until` come with the read):
// the browser's clock decides nothing.
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';

import { SESSION_KEY } from '../session/session';
import { ApiFailure, call, callNoContent } from './client';
import { useApi } from './context';
import type { DocLintMigration } from './types';

export const docLintKeys = {
  migration: (project: number) => ['doc-lint-migration', project] as const,
};

export function useDocLintMigration(project: number) {
  const api = useApi();
  return useQuery({
    queryKey: docLintKeys.migration(project),
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/doc-lint/migration', { signal, params: { path: { id: project } } }),
      ),
  });
}

/** A 403 on a save is most often a stale CSRF token: read the session again. */
function useRefreshSessionOn403() {
  const queryClient = useQueryClient();
  return (error: unknown) => {
    if (error instanceof ApiFailure && error.failure.kind === 'api' && error.failure.status === 403) {
      void queryClient.invalidateQueries({ queryKey: SESSION_KEY });
    }
  };
}

export function useSetDocLintMigration(project: number) {
  const api = useApi();
  const queryClient = useQueryClient();
  const refreshSession = useRefreshSessionOn403();
  return useMutation({
    mutationFn: (warnUntil: string) =>
      call(() =>
        api.PUT('/api/v1/projects/{id}/doc-lint/migration', {
          params: { path: { id: project } },
          body: { warn_until: warnUntil },
        }),
      ),
    onSuccess: (next: DocLintMigration) => {
      queryClient.setQueryData(docLintKeys.migration(project), next);
    },
    onError: refreshSession,
  });
}

export function useClearDocLintMigration(project: number) {
  const api = useApi();
  const queryClient = useQueryClient();
  const refreshSession = useRefreshSessionOn403();
  return useMutation({
    mutationFn: () =>
      callNoContent(() =>
        api.DELETE('/api/v1/projects/{id}/doc-lint/migration', { params: { path: { id: project } } }),
      ),
    onSuccess: async () => {
      await queryClient.invalidateQueries({ queryKey: docLintKeys.migration(project) });
    },
    onError: refreshSession,
  });
}
