// A project's ai_assets.enabled (plan 17 P17-3, ADR-0035): the opt-in of the
// AI asset lint, the forge/risk rules ai_case_raw_data (stop) / ai_asset
// (warn) and asset releases. Keys start with ['aiAssets', projectId].
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';

import { SESSION_KEY } from '../session/session';
import { ApiFailure, call } from './client';
import { useApi } from './context';
import type { components } from './schema.gen';

export type AiAssets = components['schemas']['AiAssetsBody'];

export const aiAssetsKeys = {
  project: (project: number) => ['aiAssets', project] as const,
};

export function useAiAssets(project: number) {
  const api = useApi();
  return useQuery({
    queryKey: aiAssetsKeys.project(project),
    queryFn: ({ signal }) =>
      call(() => api.GET('/api/v1/projects/{id}/ai-assets', { signal, params: { path: { id: project } } })),
  });
}

export function useSetAiAssets(project: number) {
  const api = useApi();
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (enabled: boolean) =>
      call(() =>
        api.PUT('/api/v1/projects/{id}/ai-assets', {
          params: { path: { id: project } },
          body: { enabled },
        }),
      ),
    onSuccess: (next: AiAssets) => {
      queryClient.setQueryData(aiAssetsKeys.project(project), next);
    },
    // A 403 on a save is most often a stale CSRF token: read the session again.
    onError: (error: unknown) => {
      if (error instanceof ApiFailure && error.failure.kind === 'api' && error.failure.status === 403) {
        void queryClient.invalidateQueries({ queryKey: SESSION_KEY });
      }
    },
  });
}
