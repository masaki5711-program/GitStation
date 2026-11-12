// The editor's save dialog (the shared Modal): the commit message (a
// default from the path), an optional description, and where the commit goes
// — a new branch with a merge request (the default) or the branch itself
// (offered disabled, with the reason, when a protected-branch rule refuses
// it). A refusal stays in the dialog with the server's user_message and
// request_id; a conflict, a secret and the rate limit get their own words and
// next steps. Esc / Cancel close it; the editor puts the focus back.
import { useQueryClient } from '@tanstack/react-query';
import { useId, useState, type SyntheticEvent } from 'react';
import { useTranslation } from 'react-i18next';

import { failureOf } from '../../../api/client';
import {
  CODE_CONFLICT,
  CODE_RATE_LIMIT,
  CODE_SECRET,
  commitErrorCode,
  useSingleFileCommit,
  type SingleFileCommit,
  type SingleFileCommitResult,
} from '../../../api/fileCommits';
import { useProtectedRefs } from '../../../api/mergeSettings';
import type { Rev } from '../../../api/types';
import { CopyButton } from '../../../components/CopyButton';
import { ErrorNotice } from '../../../components/ErrorNotice';
import { TextField } from '../../../components/Field';
import controls from '../../../components/controls.module.css';
import { branchNameFor, directCommitBlock } from '../../../features/editorModel';
import { blobUrl } from '../../../features/repoUrls';
import { Modal } from '../../../ui/Modal';
import type { EditorMode } from './EditorScreen';
import styles from './Editor.module.css';

type Target = 'branch' | 'direct';

export function SaveDialog(props: {
  open: boolean;
  projectId: number;
  project: string;
  rev: Rev;
  mode: EditorMode;
  path: string;
  text: string;
  role: string | null;
  isAdmin: boolean;
  onCancel: () => void;
  onSaved: (result: SingleFileCommitResult) => void;
}) {
  const { t } = useTranslation();
  const save = useSingleFileCommit(props.projectId);
  // A refusal belongs to one attempt: closing forgets it.
  const close = () => {
    save.reset();
    props.onCancel();
  };
  return (
    <Modal open={props.open} onClose={close} title={t('editor.dialog.title')} busy={save.isPending}>
      <SaveForm {...props} onCancel={close} save={save} />
    </Modal>
  );
}

function SaveForm({
  projectId,
  project,
  rev,
  mode,
  path,
  text,
  role,
  isAdmin,
  onCancel,
  onSaved,
  save,
}: {
  projectId: number;
  project: string;
  rev: Rev;
  mode: EditorMode;
  path: string;
  text: string;
  role: string | null;
  isAdmin: boolean;
  onCancel: () => void;
  onSaved: (result: SingleFileCommitResult) => void;
  save: ReturnType<typeof useSingleFileCommit>;
}) {
  const { t } = useTranslation();
  const ids = useId();
  const rules = useProtectedRefs(projectId);
  const block = directCommitBlock(rules.data?.rules, rev.name, role, isAdmin);
  const [message, setMessage] = useState(() =>
    mode === 'edit' ? t('editor.dialog.defaultUpdate', { path }) : t('editor.dialog.defaultAdd', { path }),
  );
  const [describe, setDescribe] = useState(false);
  const [description, setDescription] = useState('');
  const [target, setTarget] = useState<Target>('branch');
  const [branch, setBranch] = useState(() => branchNameFor(path, new Date()));
  const [submitted, setSubmitted] = useState(false);

  const messageError = submitted && message.trim() === '' ? t('editor.dialog.messageRequired') : null;
  const branchError =
    submitted && target === 'branch' && branch.trim() === ''
      ? t('editor.dialog.branchRequired')
      : submitted && target === 'branch' && branch.trim() === rev.name
        ? t('editor.dialog.branchSame')
        : null;
  const directDisabled = block !== null;
  const effectiveTarget: Target = directDisabled ? 'branch' : target;

  const submit = (e: SyntheticEvent) => {
    e.preventDefault();
    if (save.isPending) return;
    setSubmitted(true);
    const subject = message.trim();
    const name = branch.trim();
    if (subject === '' || (effectiveTarget === 'branch' && (name === '' || name === rev.name))) return;
    const body = description.trim();
    const payload: SingleFileCommit = {
      branch: rev.full_name ?? rev.name,
      start_sha: rev.commit,
      path,
      content: text,
      message: describe && body !== '' ? `${subject}\n\n${body}` : subject,
      ...(effectiveTarget === 'branch' ? { create_branch: name, open_merge_request: true } : {}),
    };
    save.mutate(payload, { onSuccess: onSaved });
  };

  const directReason =
    block === 'forbidden'
      ? t('editor.dialog.directForbidden')
      : block === 'role'
        ? t('editor.dialog.directRole')
        : t('editor.dialog.directHelp', { branch: rev.name });

  return (
    <form onSubmit={submit} noValidate className={styles.dialogForm} aria-busy={save.isPending}>
      {save.isError && (
        <CommitError
          error={save.error}
          projectId={projectId}
          project={project}
          rev={rev}
          path={path}
          text={text}
          branchMode={effectiveTarget === 'branch'}
        />
      )}
      <TextField
        label={t('editor.dialog.message')}
        value={message}
        onChange={setMessage}
        error={messageError}
        required
        initialFocus
      />
      <p className={styles.toggleRow}>
        <button
          type="button"
          className={`${controls.ghost} ${controls.small}`}
          aria-expanded={describe}
          onClick={() => {
            setDescribe((v) => !v);
          }}
        >
          {t('editor.dialog.addDescription')}
        </button>
      </p>
      {describe && (
        <TextField
          label={t('editor.dialog.description')}
          value={description}
          onChange={setDescription}
          help={t('editor.dialog.descriptionHelp')}
          multiline
        />
      )}

      <fieldset className={controls.field}>
        <legend className={controls.label}>{t('editor.dialog.target')}</legend>
        <div className={styles.choice}>
          <label className={controls.choiceLabel}>
            <input
              type="radio"
              name={`${ids}-target`}
              value="branch"
              checked={effectiveTarget === 'branch'}
              aria-describedby={`${ids}-branch-help`}
              onChange={() => {
                setTarget('branch');
              }}
            />
            <span>{t('editor.dialog.targetBranch')}</span>
          </label>
          <p id={`${ids}-branch-help`} className={controls.choiceHelp}>
            {t('editor.dialog.targetBranchHelp', { branch: rev.name })}
          </p>
          {effectiveTarget === 'branch' && (
            <div className={styles.branchField}>
              <TextField
                label={t('editor.dialog.branchName')}
                value={branch}
                onChange={setBranch}
                error={branchError}
                required
              />
            </div>
          )}
        </div>
        <div className={styles.choice}>
          <label className={controls.choiceLabel}>
            <input
              type="radio"
              name={`${ids}-target`}
              value="direct"
              checked={effectiveTarget === 'direct'}
              disabled={directDisabled}
              aria-describedby={`${ids}-direct-help`}
              onChange={() => {
                setTarget('direct');
              }}
            />
            <span>{t('editor.dialog.targetDirect', { branch: rev.name })}</span>
          </label>
          <p id={`${ids}-direct-help`} className={controls.choiceHelp}>
            {directReason}
          </p>
        </div>
      </fieldset>

      <div className={styles.dialogActions}>
        <button type="button" className={controls.ghost} disabled={save.isPending} onClick={onCancel}>
          {t('common.cancel')}
        </button>
        <button
          type="submit"
          className={controls.primary}
          aria-disabled={save.isPending ? 'true' : undefined}
          aria-busy={save.isPending ? 'true' : undefined}
        >
          {save.isPending
            ? t('editor.dialog.saving')
            : effectiveTarget === 'branch'
              ? t('editor.dialog.submitBranch')
              : t('editor.dialog.submitDirect')}
        </button>
      </div>
    </form>
  );
}

/** A refused save: the server's words and request id, with the next steps
 * the editor can offer for the refusals it knows. */
function CommitError({
  error,
  projectId,
  project,
  rev,
  path,
  text,
  branchMode,
}: {
  error: unknown;
  projectId: number;
  project: string;
  rev: Rev;
  path: string;
  text: string;
  branchMode: boolean;
}) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const failure = failureOf(error);
  const code = commitErrorCode(error);
  if (code === CODE_CONFLICT) {
    return (
      <div className={styles.errorBlock} data-testid="editor-conflict">
        <ErrorNotice failure={failure} title={t('editor.error.conflictTitle')} />
        <p>{t('editor.error.conflictNext')}</p>
        {branchMode && <p className={styles.hint}>{t('editor.error.conflictBranch')}</p>}
        <div className={styles.nextActions}>
          <CopyButton text={text} label={t('editor.error.copyMine')} />
          <a
            className={controls.button}
            href={blobUrl(project, rev.name, path)}
            target="_blank"
            rel="noopener noreferrer"
          >
            {t('editor.error.openLatest')}
          </a>
          <button
            type="button"
            className={controls.danger}
            onClick={() => {
              // A new commit at the address remounts the editor on the latest file.
              void queryClient.invalidateQueries({ queryKey: ['resolve-spec', projectId] });
              void queryClient.invalidateQueries({ queryKey: ['refs', projectId] });
            }}
          >
            {t('editor.error.reloadLatest')}
          </button>
        </div>
      </div>
    );
  }
  if (code === CODE_SECRET) {
    return (
      <div className={styles.errorBlock} data-testid="editor-secret">
        <ErrorNotice failure={failure} title={t('editor.error.secretTitle')} />
        <p>{t('editor.error.secretNext')}</p>
      </div>
    );
  }
  if (code === CODE_RATE_LIMIT) {
    return (
      <div className={styles.errorBlock} data-testid="editor-rate-limited">
        <ErrorNotice failure={failure} title={t('editor.error.rateLimitTitle')} />
        <p>{t('editor.error.rateLimitNext')}</p>
      </div>
    );
  }
  return (
    <div className={styles.errorBlock}>
      <ErrorNotice failure={failure} title={t('editor.error.title')} />
    </div>
  );
}
