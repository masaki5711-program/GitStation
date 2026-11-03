// Open a merge request (`/-/pulls/new`): the two branches come from the
// repository's refs, the target starts at the default branch. The server
// decides everything that matters (both branches exist, rights, an MR already
// open); the form only refuses the obvious mistakes before sending. A refusal
// is shown as the server's user_message + request_id and the form keeps what
// was typed.
import { useState, type SyntheticEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { Link, useNavigate } from 'react-router';

import { useCreateMergeRequest } from '../../../api/mergeRequests';
import { useRefs } from '../../../api/repo';
import type { CreateMergeRequestBody, RefsBody } from '../../../api/types';
import { SelectField, TextField } from '../../../components/Field';
import { Loading } from '../../../components/Loading';
import { QueryError } from '../../../components/QueryError';
import controls from '../../../components/controls.module.css';
import { ErrorSummary } from '../../../features/CreateDialogs';
import { pullUrl, pullsUrl } from '../../../features/repoUrls';
import { useDocumentTitle } from '../../../hooks/useDocumentTitle';
import { RepoError } from '../common';
import repo from '../Repo.module.css';
import styles from './Mr.module.css';

const TITLE_MAX = 255;
/** Characters as the server counts them (Unicode scalar values). */
const length = (text: string) => Array.from(text).length;

export function NewMergeRequestScreen({
  projectId,
  project,
}: {
  projectId: number;
  project: string;
}) {
  const { t } = useTranslation();
  const refs = useRefs(projectId);
  const title = t('mr.new.title');
  useDocumentTitle(title);

  let body;
  if (refs.isPending) body = <Loading />;
  else if (refs.isError) {
    body = (
      <RepoError
        error={refs.error}
        onRetry={() => {
          void refs.refetch();
        }}
      />
    );
  } else if (!refs.data.refs.some((r) => r.type === 'branch')) {
    body = (
      <>
        <p>{t('mr.new.noBranches')}</p>
        <p>
          <Link to={`/${project.split('/').map(encodeURIComponent).join('/')}`}>
            {t('repo.backToProject')}
          </Link>
        </p>
      </>
    );
  } else {
    body = <CreateForm projectId={projectId} project={project} refs={refs.data} />;
  }
  return (
    <>
      <h1 className={repo.title}>{title}</h1>
      {body}
    </>
  );
}

function CreateForm({
  projectId,
  project,
  refs,
}: {
  projectId: number;
  project: string;
  refs: RefsBody;
}) {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const branches = refs.refs.filter((r) => r.type === 'branch');
  const [source, setSource] = useState('');
  const [target, setTarget] = useState(refs.default_branch ?? '');
  const [title, setTitle] = useState('');
  const [description, setDescription] = useState('');
  const [submitted, setSubmitted] = useState(false);
  const [attempt, setAttempt] = useState(0);
  const create = useCreateMergeRequest(projectId);

  const trimmed = title.trim();
  const sourceError = !submitted
    ? null
    : source === ''
      ? t('mr.new.sourceRequired')
      : source === target
        ? t('mr.new.sameBranch')
        : null;
  const targetError = submitted && target === '' ? t('mr.new.targetRequired') : null;
  const titleError = !submitted
    ? null
    : trimmed === ''
      ? t('mr.new.titleRequired')
      : length(trimmed) > TITLE_MAX
        ? t('mr.new.titleTooLong')
        : null;
  const errors = [sourceError, targetError, titleError].filter((e): e is string => e !== null);

  const submit = (e: SyntheticEvent) => {
    e.preventDefault();
    // aria-disabled (not disabled) keeps the focus on the button while saving.
    if (create.isPending) return;
    setSubmitted(true);
    setAttempt((n) => n + 1);
    const ok =
      source !== '' &&
      target !== '' &&
      source !== target &&
      trimmed !== '' &&
      length(trimmed) <= TITLE_MAX;
    if (!ok) return;
    const text = description.trim();
    const payload: CreateMergeRequestBody = {
      source_branch: source,
      target_branch: target,
      title: trimmed,
      ...(text === '' ? {} : { description: text }),
    };
    create.mutate(payload, {
      onSuccess: (created) => {
        void navigate(pullUrl(project, created.iid));
      },
    });
  };

  const options = branches.map((r) => (
    <option key={r.full_name} value={r.name}>
      {r.name === refs.default_branch ? t('mr.new.defaultBranch', { name: r.name }) : r.name}
    </option>
  ));

  return (
    <form onSubmit={submit} noValidate className={styles.form}>
      <ErrorSummary errors={errors} attempt={attempt} />
      <SelectField
        label={t('mr.new.source')}
        value={source}
        onChange={setSource}
        error={sourceError}
      >
        <option value="">{t('mr.new.choose')}</option>
        {options}
      </SelectField>
      <SelectField
        label={t('mr.new.target')}
        value={target}
        onChange={setTarget}
        error={targetError}
      >
        <option value="">{t('mr.new.choose')}</option>
        {options}
      </SelectField>
      {refs.truncated && <p className={repo.hint}>{t('repo.refsTruncated')}</p>}
      <TextField
        label={t('mr.new.titleLabel')}
        value={title}
        onChange={setTitle}
        error={titleError}
        required
      />
      <TextField
        label={t('mr.new.description')}
        value={description}
        onChange={setDescription}
        multiline
      />
      {create.isError && <QueryError error={create.error} />}
      <div className={styles.formActions}>
        <button
          type="submit"
          className={controls.primary}
          aria-disabled={create.isPending ? 'true' : undefined}
        >
          {create.isPending ? t('create.creating') : t('mr.new.submit')}
        </button>
        <Link to={pullsUrl(project)}>{t('common.cancel')}</Link>
      </div>
    </form>
  );
}
