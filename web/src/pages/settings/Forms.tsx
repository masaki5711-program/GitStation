// Forms the project and the group settings share: name + description, and
// move / rename. The server decides everything that matters (rights, rate
// limits, tombstones); a refusal is shown as its reason + request id and the
// input stays.
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { useState, type SyntheticEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { useNavigate } from 'react-router';

import { call } from '../../api/client';
import { useApi } from '../../api/context';
import { groupsQuery } from '../../api/lists';
import type { components } from '../../api/schema.gen';
import { SelectField, TextField } from '../../components/Field';
import controls from '../../components/controls.module.css';
import { ErrorSummary } from '../../features/CreateDialogs';
import { isValidSegment } from '../../features/paths';
import { useSession } from '../../session/session';
import { Denied, useRefreshSessionOn403 } from './common';
import styles from './Settings.module.css';

/** Name and description (`PATCH`); only what changed is sent. */
export function GeneralForm({
  displayName,
  description,
  save,
}: {
  displayName: string;
  description: string;
  save: (body: { display_name?: string; description?: string }) => Promise<unknown>;
}) {
  const { t } = useTranslation();
  const [name, setName] = useState(displayName);
  const [text, setText] = useState(description);
  const [attempt, setAttempt] = useState(0);
  const refresh = useRefreshSessionOn403();
  const mutation = useMutation({
    mutationFn: save,
    onError: refresh,
  });
  const nameError = name.trim() === '' ? t('form.nameRequired') : null;
  const errors = nameError === null ? [] : [nameError];
  const submit = (e: SyntheticEvent) => {
    e.preventDefault();
    if (mutation.isPending) return;
    setAttempt((n) => n + 1);
    if (nameError !== null) return;
    const body: { display_name?: string; description?: string } = {};
    if (name.trim() !== displayName) body.display_name = name.trim();
    if (text.trim() !== description) body.description = text.trim();
    if (Object.keys(body).length === 0) return;
    mutation.mutate(body);
  };
  return (
    <form onSubmit={submit} noValidate className={styles.narrow}>
      <ErrorSummary errors={errors} attempt={attempt} />
      <TextField
        label={t('form.displayName')}
        value={name}
        onChange={setName}
        error={attempt > 0 ? nameError : null}
        required
      />
      <TextField label={t('form.description')} value={text} onChange={setText} multiline />
      {mutation.isError && <Denied error={mutation.error} />}
      <button
        type="submit"
        className={controls.primary}
        aria-disabled={mutation.isPending ? 'true' : undefined}
      >
        {mutation.isPending ? t('settings.saving') : t('settings.save')}
      </button>
      <span role="status" className={styles.muted}>
        {mutation.isSuccess ? t('settings.saved') : ''}
      </span>
    </form>
  );
}

type Moved = components['schemas']['MoveBody'];

/** Move and rename: a new parent (groups the person may see, or their own
 * namespace) and / or a new last path segment. The old address keeps leading
 * to the new one (a tombstone), and the number of moves in a short time is
 * limited — said before the request, not only after a refusal. */
export function MoveForm({
  kind,
  id,
  path,
}: {
  kind: 'project' | 'group';
  id: number;
  /** The current full path. */
  path: string;
}) {
  const { t } = useTranslation();
  const api = useApi();
  const session = useSession();
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const refresh = useRefreshSessionOn403();
  const last = path.split('/').pop() ?? path;
  const [target, setTarget] = useState('');
  const [segment, setSegment] = useState(last);
  const [reclaim, setReclaim] = useState(false);
  const [attempt, setAttempt] = useState(0);

  const groups = useQuery(groupsQuery(api, { membership: true }));
  const personal = useQuery({
    queryKey: ['namespace', session.user.username],
    enabled: kind === 'project',
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/namespaces/resolve', {
          signal,
          params: { query: { path: session.user.username } },
        }),
      ),
  });

  const move = useMutation({
    mutationFn: (body: { namespace_id?: number; top_level?: boolean; path?: string; reclaim: boolean }) =>
      kind === 'project'
        ? call(() =>
            api.POST('/api/v1/projects/{id}/transfer', {
              params: { path: { id } },
              body: {
                reclaim: body.reclaim,
                ...(body.namespace_id === undefined ? {} : { namespace_id: body.namespace_id }),
                ...(body.path === undefined ? {} : { path: body.path }),
              },
            }),
          )
        : call(() =>
            api.POST('/api/v1/groups/{id}/transfer', {
              params: { path: { id } },
              body: {
                reclaim: body.reclaim,
                ...(body.namespace_id === undefined ? {} : { namespace_id: body.namespace_id }),
                ...(body.top_level === true ? { top_level: true } : {}),
                ...(body.path === undefined ? {} : { path: body.path }),
              },
            }),
          ),
    onSuccess: async (result: Moved) => {
      await queryClient.invalidateQueries();
      void navigate(`/${result.path}/-/settings`, {
        replace: true,
        state: { movedFrom: result.old_path },
      });
    },
    onError: refresh,
  });

  const segmentError = segment !== last && !isValidSegment(segment) ? t('form.pathInvalid') : null;
  const unchanged = target === '' && segment === last;
  const nothingError = attempt > 0 && unchanged ? t('settings.move.nothing') : null;
  const errors = [segmentError, nothingError].filter((e): e is string => e !== null);

  const submit = (e: SyntheticEvent) => {
    e.preventDefault();
    if (move.isPending) return;
    setAttempt((n) => n + 1);
    if (segmentError !== null || unchanged) return;
    move.mutate({
      reclaim,
      ...(target === 'top' ? { top_level: true } : {}),
      ...(target !== '' && target !== 'top' ? { namespace_id: Number(target) } : {}),
      ...(segment !== last ? { path: segment } : {}),
    });
  };

  return (
    <form onSubmit={submit} noValidate className={styles.narrow}>
      <p className={styles.hint}>{t('settings.move.explain')}</p>
      <p className={styles.hint}>{t('settings.move.limit')}</p>
      <ErrorSummary errors={errors} attempt={attempt} />
      <SelectField label={t('settings.move.target')} value={target} onChange={setTarget}>
        <option value="">{t('settings.move.keepParent')}</option>
        {kind === 'group' && session.user.is_admin && (
          <option value="top">{t('settings.move.topLevel')}</option>
        )}
        {kind === 'project' && personal.data && (
          <option value={String(personal.data.namespace_id)}>
            {session.user.username} {t('create.parentPersonal')}
          </option>
        )}
        {groups.data?.items
          .filter((g) => !(kind === 'group' && (g.id === id || g.path.startsWith(`${path}/`))))
          .map((g) => (
            <option key={g.id} value={String(g.namespace_id)}>
              {g.path}
            </option>
          ))}
      </SelectField>
      <TextField
        label={t('settings.move.segment')}
        value={segment}
        onChange={setSegment}
        error={segmentError}
        help={t('form.pathHelp')}
      />
      <div className={controls.field}>
        <label className={styles.check}>
          <input
            type="checkbox"
            checked={reclaim}
            onChange={(e) => {
              setReclaim(e.target.checked);
            }}
          />
          <span>{t('settings.move.reclaim')}</span>
        </label>
        <p className={controls.help}>{t('settings.move.reclaimHelp')}</p>
      </div>
      {move.isError && <Denied error={move.error} />}
      <button
        type="submit"
        className={controls.primary}
        aria-disabled={move.isPending ? 'true' : undefined}
      >
        {move.isPending ? t('settings.saving') : t('settings.move.submit')}
      </button>
    </form>
  );
}
