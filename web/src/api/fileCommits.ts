// The simple editor's one write: `POST …/repository/commits/single-file`
// (docs/api/single-file-commit.md). The answer is a commit on a new branch
// (with a merge request by default) or on the branch itself.
//
// The editor reacts differently to a few refusals (a conflict offers the
// latest version, a secret says to remove it, the rate limit says to wait),
// so a failure also carries the server's `error_code`. The code is used only
// to choose the words around the server's `user_message`; it is never shown
// (CLAUDE.md contract 3).
import { useMutation, useQueryClient } from '@tanstack/react-query';

import { ApiFailure, call, isRecord } from './client';
import { useApi } from './context';
import type { components } from './schema.gen';

export type SingleFileCommit = components['schemas']['SingleFileCommit'];
export type SingleFileCommitResult = components['schemas']['SingleFileCommitBody'];

/** The codes the editor words differently (errors.toml). */
export const CODE_CONFLICT = 'FG-GIT-0027';
export const CODE_SECRET = 'FG-GIT-0022';
export const CODE_RATE_LIMIT = 'FG-WEB-0010';

/** The largest content the server takes (512 KiB; the push blob limit may be lower). */
export const MAX_CONTENT_BYTES = 512 * 1024;

/** An [`ApiFailure`] with the server's error code (for choosing words only). */
export class CommitFailure extends ApiFailure {
  readonly code: string | null;

  constructor(base: ApiFailure, code: string | null) {
    super(base.failure, base);
    this.name = 'CommitFailure';
    this.code = code;
  }
}

/** The code of a failed commit, `null` for anything else. */
export function commitErrorCode(error: unknown): string | null {
  return error instanceof CommitFailure ? error.code : null;
}

const ROLE_RANK: Record<string, number> = {
  guest: 0,
  reporter: 1,
  developer: 2,
  maintainer: 3,
  owner: 4,
};

/** The rank of a role (`-1`: none or unknown). */
export function roleRank(role: string | null | undefined): number {
  return role === null || role === undefined ? -1 : (ROLE_RANK[role] ?? -1);
}

/** Whether the screens offer editing: Developer or above, or an administrator.
 * The server decides again (`RepoPush`, the protected-branch rules). */
export function mayPush(role: string | null | undefined, isAdmin: boolean): boolean {
  return isAdmin || roleRank(role) >= roleRank('developer');
}

export function useSingleFileCommit(projectId: number) {
  const api = useApi();
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: async (body: SingleFileCommit): Promise<SingleFileCommitResult> => {
      let code: string | null = null;
      try {
        return await call(async () => {
          const result = await api.POST('/api/v1/projects/{id}/repository/commits/single-file', {
            params: { path: { id: projectId } },
            body,
          });
          const error: unknown = result.error;
          if (isRecord(error) && typeof error.error_code === 'string') code = error.error_code;
          return result;
        });
      } catch (e) {
        if (e instanceof ApiFailure) throw new CommitFailure(e, code);
        throw e;
      }
    },
    onSuccess: () => {
      // The branch moved (or was created) and the file changed: every browse
      // answer of the project may be stale. Marked only (no refetch now): the
      // editor must not reload under the person; the next screen reads anew.
      for (const word of ['refs', 'resolve-spec', 'tree', 'blob', 'commits']) {
        void queryClient.invalidateQueries({ queryKey: [word, projectId], refetchType: 'none' });
      }
    },
  });
}
