// Create a project / a group in a native <dialog> (plan 10 §4.3). The server
// decides everything that matters (reserved words, taken names, rights); the
// form only spares the obvious round trips. A failure is shown as
// user_message + request_id (ErrorNotice) and the form keeps what was typed.
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { useEffect, useRef, useState, type SyntheticEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { useNavigate } from 'react-router';

import { ApiFailure, call } from '../api/client';
import { useApi } from '../api/context';
import { groupsQuery } from '../api/lists';
import type { Kind, Visibility } from '../api/types';
import { RadioField, SelectField, TextField } from '../components/Field';
import { QueryError } from '../components/QueryError';
import controls from '../components/controls.module.css';
import { SESSION_KEY, useSession } from '../session/session';
import { Modal } from '../ui/Modal';
import { isValidSegment, suggestSegment } from './paths';
import styles from './CreateDialogs.module.css';

export interface Parent {
  namespaceId: number;
  /** The parent's path, shown to the person. */
  path: string;
}

interface DialogProps {
  open: boolean;
  onClose: () => void;
}

/** One form state for both dialogs: name, path (suggested until typed), text. */
function useNamePath() {
  const [displayName, setDisplayName] = useState('');
  const [path, setPath] = useState('');
  const [pathTouched, setPathTouched] = useState(false);
  const [description, setDescription] = useState('');
  return {
    displayName,
    path,
    description,
    setDescription,
    setDisplayName: (v: string) => {
      setDisplayName(v);
      if (!pathTouched) setPath(suggestSegment(v));
    },
    setPath: (v: string) => {
      setPathTouched(true);
      setPath(v);
    },
  };
}


/** Invalid submit: this summary takes the focus and is announced (WCAG 3.3.1,
 * 4.1.3); the fields below carry aria-invalid and their own message. */
function ErrorSummary({ errors, attempt }: { errors: string[]; attempt: number }) {
  const { t } = useTranslation();
  const ref = useRef<HTMLDivElement>(null);
  const has = errors.length > 0;
  useEffect(() => {
    if (attempt > 0 && has) ref.current?.focus();
  }, [attempt, has]);
  if (attempt === 0 || !has) return null;
  return (
    <div ref={ref} tabIndex={-1} role="alert" className={styles.summary}>
      <p className={styles.summaryTitle}>{t('form.errorSummary')}</p>
      <ul>
        {errors.map((e) => (
          <li key={e}>{e}</li>
        ))}
      </ul>
    </div>
  );
}

/** Tell the dialog a save is under way (it then ignores Close and Esc). */
function useReportBusy(pending: boolean, onBusy: (busy: boolean) => void) {
  useEffect(() => {
    onBusy(pending);
    return () => {
      onBusy(false);
    };
  }, [pending, onBusy]);
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

export function CreateProjectDialog({
  open,
  onClose,
  parent,
}: DialogProps & { parent?: Parent }) {
  const { t } = useTranslation();
  const [busy, setBusy] = useState(false);
  return (
    <Modal open={open} onClose={onClose} title={t('create.project.title')} busy={busy}>
      <CreateProjectForm onClose={onClose} onBusy={setBusy} {...(parent ? { parent } : {})} />
    </Modal>
  );
}

function CreateProjectForm({
  onClose,
  onBusy,
  parent,
}: {
  onClose: () => void;
  onBusy: (busy: boolean) => void;
  parent?: Parent;
}) {
  const { t } = useTranslation();
  const api = useApi();
  const session = useSession();
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const f = useNamePath();
  const [parentId, setParentId] = useState<string>(parent ? String(parent.namespaceId) : '');
  const [visibility, setVisibility] = useState<Visibility>('private');
  const [kind, setKind] = useState<Kind>('system');
  const [submitted, setSubmitted] = useState(false);
  const [attempt, setAttempt] = useState(0);
  const refreshSession = useRefreshSessionOn403();

  // The choices of parent: my own namespace, then the groups I belong to.
  const groups = useQuery({ ...groupsQuery(api, { membership: true }), enabled: !parent });
  const personal = useQuery({
    queryKey: ['namespace', session.user.username],
    enabled: !parent,
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/namespaces/resolve', {
          signal,
          params: { query: { path: session.user.username } },
        }),
      ),
  });

  const create = useMutation({
    mutationFn: (body: { namespace_id: number }) =>
      call(() =>
        api.POST('/api/v1/projects', {
          body: {
            namespace_id: body.namespace_id,
            display_name: f.displayName.trim(),
            path: f.path,
            description: f.description.trim() === '' ? null : f.description.trim(),
            visibility,
            kind,
          },
        }),
      ),
    onSuccess: async (project) => {
      await queryClient.invalidateQueries({ queryKey: ['projects'] });
      onClose();
      void navigate(`/${project.path}`);
    },
    onError: refreshSession,
  });
  useReportBusy(create.isPending, onBusy);

  const nameError = submitted && f.displayName.trim() === '' ? t('form.nameRequired') : null;
  const pathError = submitted && !isValidSegment(f.path) ? t('form.pathInvalid') : null;
  const parentError = submitted && parentId === '' ? t('form.parentRequired') : null;

  const errors = [parentError, nameError, pathError].filter((e): e is string => e !== null);

  const submit = (e: SyntheticEvent) => {
    e.preventDefault();
    // aria-disabled (not disabled) keeps the focus on the button while saving.
    if (create.isPending) return;
    setSubmitted(true);
    setAttempt((n) => n + 1);
    if (f.displayName.trim() === '' || !isValidSegment(f.path) || parentId === '') return;
    create.mutate({ namespace_id: Number(parentId) });
  };

  const choicesLoading = !parent && (groups.isPending || personal.isPending);

  return (
    <form onSubmit={submit} noValidate>
      <ErrorSummary errors={errors} attempt={attempt} />
      {parent ? (
        <p>
          {t('create.parent')}: <code>{parent.path}</code>
        </p>
      ) : (
        <SelectField
          label={t('create.parent')}
          value={parentId}
          onChange={setParentId}
          error={parentError}
        >
          <option value="">{choicesLoading ? t('common.loading') : t('create.parentChoose')}</option>
          {personal.data && (
            <option value={String(personal.data.namespace_id)}>
              {session.user.username} {t('create.parentPersonal')}
            </option>
          )}
          {groups.data?.items.map((g) => (
            <option key={g.id} value={String(g.namespace_id)}>
              {g.path}
            </option>
          ))}
        </SelectField>
      )}
      <TextField
        label={t('form.displayName')}
        value={f.displayName}
        onChange={f.setDisplayName}
        error={nameError}
        required
        initialFocus
      />
      <TextField
        label={t('form.path')}
        value={f.path}
        onChange={f.setPath}
        error={pathError}
        help={t('form.pathHelp')}
        required
      />
      <TextField
        label={t('form.description')}
        value={f.description}
        onChange={f.setDescription}
        multiline
      />
      <RadioField<Visibility>
        legend={t('form.visibility')}
        name="visibility"
        value={visibility}
        onChange={setVisibility}
        choices={[
          {
            value: 'private',
            label: t('visibility.private'),
            help: t('visibility.privateHelp'),
          },
          {
            value: 'internal',
            label: t('visibility.internal'),
            help: t('visibility.internalHelp'),
          },
        ]}
      />
      <RadioField<Kind>
        legend={t('form.kind')}
        name="kind"
        value={kind}
        onChange={setKind}
        choices={[
          { value: 'system', label: t('kind.system'), help: t('kind.systemHelp') },
          { value: 'spec', label: t('kind.spec'), help: t('kind.specHelp') },
        ]}
      />
      {create.isError && <QueryError error={create.error} />}
      <button
        type="submit"
        className={controls.primary}
        aria-disabled={create.isPending ? 'true' : undefined}
      >
        {create.isPending ? t('create.creating') : t('create.project.submit')}
      </button>
    </form>
  );
}

export function CreateGroupDialog({
  open,
  onClose,
  parent,
}: DialogProps & {
  /** Given: a subgroup of this group (`groupId` is the group's id). */
  parent?: { groupId: number; path: string };
}) {
  const { t } = useTranslation();
  const [busy, setBusy] = useState(false);
  return (
    <Modal
      open={open}
      onClose={onClose}
      title={parent ? t('create.subgroup.title') : t('create.group.title')}
      busy={busy}
    >
      <CreateGroupForm onClose={onClose} onBusy={setBusy} {...(parent ? { parent } : {})} />
    </Modal>
  );
}

function CreateGroupForm({
  onClose,
  onBusy,
  parent,
}: {
  onClose: () => void;
  onBusy: (busy: boolean) => void;
  parent?: { groupId: number; path: string };
}) {
  const { t } = useTranslation();
  const api = useApi();
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const f = useNamePath();
  const [submitted, setSubmitted] = useState(false);
  const [attempt, setAttempt] = useState(0);
  const refreshSession = useRefreshSessionOn403();

  const create = useMutation({
    mutationFn: () => {
      const body = {
        display_name: f.displayName.trim(),
        path: f.path,
        description: f.description.trim() === '' ? null : f.description.trim(),
      };
      return parent
        ? call(() =>
            api.POST('/api/v1/groups/{id}/subgroups', {
              params: { path: { id: parent.groupId } },
              body,
            }),
          )
        : call(() => api.POST('/api/v1/groups', { body }));
    },
    onSuccess: async (group) => {
      await queryClient.invalidateQueries({ queryKey: ['groups'] });
      onClose();
      void navigate(`/${group.path}`);
    },
    onError: refreshSession,
  });
  useReportBusy(create.isPending, onBusy);

  const nameError = submitted && f.displayName.trim() === '' ? t('form.nameRequired') : null;
  const pathError = submitted && !isValidSegment(f.path) ? t('form.pathInvalid') : null;

  const errors = [nameError, pathError].filter((e): e is string => e !== null);

  const submit = (e: SyntheticEvent) => {
    e.preventDefault();
    // aria-disabled (not disabled) keeps the focus on the button while saving.
    if (create.isPending) return;
    setSubmitted(true);
    setAttempt((n) => n + 1);
    if (f.displayName.trim() === '' || !isValidSegment(f.path)) return;
    create.mutate();
  };

  return (
    <form onSubmit={submit} noValidate>
      <ErrorSummary errors={errors} attempt={attempt} />
      {parent && (
        <p>
          {t('create.parent')}: <code>{parent.path}</code>
        </p>
      )}
      <TextField
        label={t('form.displayName')}
        value={f.displayName}
        onChange={f.setDisplayName}
        error={nameError}
        required
        initialFocus
      />
      <TextField
        label={t('form.path')}
        value={f.path}
        onChange={f.setPath}
        error={pathError}
        help={t('form.pathHelp')}
        required
      />
      <TextField
        label={t('form.description')}
        value={f.description}
        onChange={f.setDescription}
        multiline
      />
      {create.isError && <QueryError error={create.error} />}
      <button
        type="submit"
        className={controls.primary}
        aria-disabled={create.isPending ? 'true' : undefined}
      >
        {create.isPending ? t('create.creating') : t('create.group.submit')}
      </button>
    </form>
  );
}
