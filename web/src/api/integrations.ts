// Integrations (M3 I1a, plan 02 §2): the administration of the kanflow
// integrations (administrators only; a change needs the administrator's web
// session) and a project's bindings to them (read as reading the project;
// unbinding by a Maintainer). Binding a repository is done from kanflow,
// acting for the person with their confirmation (ADR-0025 Amendment X3), so
// this module offers no bind.
//
// Query keys: ['integrations'] is the list, ['integrations', id] one and its
// bindings below it (one invalidation of the list reaches everything);
// ['integration-bindings', project] a project's bindings.
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';

import { call, callNoContent } from './client';
import { useApi } from './context';
import type { components } from './schema.gen';

type S = components['schemas'];
export type Integration = S['IntegrationBody'];
export type IntegrationBinding = S['IntegrationBindingBody'];
export type CreateIntegrationBody = S['CreateIntegration'];

export const integrationKeys = {
  list: ['integrations'] as const,
  one: (id: number) => ['integrations', id] as const,
  bindings: (id: number) => ['integrations', id, 'bindings'] as const,
  project: (project: number) => ['integration-bindings', project] as const,
};

export function useIntegrations() {
  const api = useApi();
  return useQuery({
    queryKey: integrationKeys.list,
    queryFn: ({ signal }) => call(() => api.GET('/api/v1/admin/integrations', { signal })),
  });
}

export function useIntegration(id: number) {
  const api = useApi();
  return useQuery({
    queryKey: integrationKeys.one(id),
    queryFn: ({ signal }) =>
      call(() => api.GET('/api/v1/admin/integrations/{id}', { signal, params: { path: { id } } })),
  });
}

export function useIntegrationBindings(id: number) {
  const api = useApi();
  return useQuery({
    queryKey: integrationKeys.bindings(id),
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/admin/integrations/{id}/bindings', { signal, params: { path: { id } } }),
      ),
  });
}

export function useCreateIntegration() {
  const api = useApi();
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (body: CreateIntegrationBody) =>
      call(() => api.POST('/api/v1/admin/integrations', { body })),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: integrationKeys.list }),
  });
}

/** Enable or disable one (the list and the item are both stale after it). */
export function useSetIntegrationEnabled() {
  const api = useApi();
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (v: { id: number; enabled: boolean }) =>
      call(() =>
        api.PATCH('/api/v1/admin/integrations/{id}', {
          params: { path: { id: v.id } },
          body: { enabled: v.enabled },
        }),
      ),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: integrationKeys.list }),
  });
}

export function useProjectBindings(project: number) {
  const api = useApi();
  return useQuery({
    queryKey: integrationKeys.project(project),
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/integration-bindings', {
          signal,
          params: { path: { id: project } },
        }),
      ),
  });
}

/** A Maintainer's direct unbind (`DELETE …/integration-bindings/{integration_id}`). */
export function useUnbindProject(project: number) {
  const api = useApi();
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (integration: number) =>
      callNoContent(() =>
        api.DELETE('/api/v1/projects/{id}/integration-bindings/{integration_id}', {
          params: { path: { id: project, integration_id: integration } },
        }),
      ),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: integrationKeys.project(project) }),
  });
}
