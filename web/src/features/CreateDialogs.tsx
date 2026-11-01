// Create a project / a group in a native <dialog> (plan 10 §4.3). The server
// decides everything that matters (reserved words, taken names, rights); the
// form only spares the obvious round trips. A failure is shown as
// user_message + request_id (ErrorNotice) and the form keeps what was typed.
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { useState, type SyntheticEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { useNavigate } from 'react-router';

import { call } from '../api/client';
import { useApi } from '../api/context';
import { groupsQuery } from '../api/lists';
import type { Kind, Visibility } from '../api/types';
import { RadioField, SelectField, TextField } from '../components/Field';
import { QueryError } from '../components/QueryError';
import controls from '../components/controls.module.css';
import { useSession } from '../session/session';
import { Modal } from '../ui/Modal';
import { isValidSegment, suggestSegment } from './paths';

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

export function CreateProjectDialog({
  open,
  onClose,
  parent,
}: DialogProps & { parent?: Parent }) {
  const { t } = useTranslation();
  return (
    <Modal open={open} onClose={onClose} title={t('create.project.title')}>
      <CreateProjectForm onClose={onClose} {...(parent ? { parent } : {})} />
    </Modal>
  );
}

function CreateProjectForm({ onClose, parent }: { onClose: () => void; parent?: Parent }) {
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
  });

  const nameError = submitted && f.displayName.trim() === '' ? t('form.nameRequired') : null;
  const pathError = submitted && !isValidSegment(f.path) ? t('form.pathInvalid') : null;
  const parentError = submitted && parentId === '' ? t('form.parentRequired') : null;

  const submit = (e: SyntheticEvent) => {
    e.preventDefault();
    setSubmitted(true);
    if (f.displayName.trim() === '' || !isValidSegment(f.path) || parentId === '') return;
    create.mutate({ namespace_id: Number(parentId) });
  };

  const choicesLoading = !parent && (groups.isPending || personal.isPending);

  return (
    <form onSubmit={submit} noValidate>
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
        autoFocus
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
      <button type="submit" className={controls.primary} disabled={create.isPending}>
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
  return (
    <Modal
      open={open}
      onClose={onClose}
      title={parent ? t('create.subgroup.title') : t('create.group.title')}
    >
      <CreateGroupForm onClose={onClose} {...(parent ? { parent } : {})} />
    </Modal>
  );
}

function CreateGroupForm({
  onClose,
  parent,
}: {
  onClose: () => void;
  parent?: { groupId: number; path: string };
}) {
  const { t } = useTranslation();
  const api = useApi();
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const f = useNamePath();
  const [submitted, setSubmitted] = useState(false);

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
  });

  const nameError = submitted && f.displayName.trim() === '' ? t('form.nameRequired') : null;
  const pathError = submitted && !isValidSegment(f.path) ? t('form.pathInvalid') : null;

  const submit = (e: SyntheticEvent) => {
    e.preventDefault();
    setSubmitted(true);
    if (f.displayName.trim() === '' || !isValidSegment(f.path)) return;
    create.mutate();
  };

  return (
    <form onSubmit={submit} noValidate>
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
        autoFocus
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
      <button type="submit" className={controls.primary} disabled={create.isPending}>
        {create.isPending ? t('create.creating') : t('create.group.submit')}
      </button>
    </form>
  );
}
