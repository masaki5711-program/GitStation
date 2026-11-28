// The AI asset draft on the merge request screen (plan 17 P17-10;
// ADR-0039 amendment P17-10b), as a suggestion and nothing else:
// - labelled 「AI の下書き（提案）」 in words and a glyph (never the colour
//   alone), framed apart from the people's threads, with when it was made;
// - the draft branch and its commit (links to the repository screens), the
//   files it changes, and how many of the model's edits the checks dropped;
// - a draft branch changed or deleted since says so;
// - one line saying what to do next, and that nothing was applied.
// Every fact is the server's code-made record (never the model's words),
// shown as plain text. Drawn only for a merge request from `improve/<id>`
// (the only ones that get a draft); no control here changes anything.
import { useTranslation } from 'react-i18next';
import { Link } from 'react-router';

import { isImprovementBranch, useAiDraft, type AiDraft, type AiDraftDetail } from '../../api/aiDraft';
import { Loading } from '../../components/Loading';
import { QueryError } from '../../components/QueryError';
import { formatDateTime } from '../labels';
import { blobUrl, commitUrl, treeUrl } from '../repoUrls';
import styles from './Mr.module.css';

const TITLE_ID = 'mr-ai-draft-title';
/** Commits are named by their first characters (as elsewhere on the screen). */
const SHORT_SHA = 8;

/** A glyph for the eye only: the words beside it carry the meaning. */
const Glyph = ({ mark }: { mark: string }) => <span aria-hidden="true">{mark} </span>;

export function AiDraftSection({
  projectId,
  project,
  iid,
  sourceBranch,
  waiting,
}: {
  projectId: number;
  /** The project's path, for the links to the branch and the commit. */
  project: string;
  iid: number;
  sourceBranch: string;
  /** The merge request is open and a draft: a draft may still come. */
  waiting: boolean;
}) {
  const { t } = useTranslation();
  const improvement = isImprovementBranch(sourceBranch);
  const read = useAiDraft(projectId, iid, improvement, waiting);
  if (!improvement) return null;
  return (
    <section
      aria-labelledby={TITLE_ID}
      className={styles.detailSection}
      data-testid="mr-ai-draft"
      data-state={read.data?.state ?? (read.isError ? 'error' : 'loading')}
    >
      <h2 id={TITLE_ID} className={styles.detailTitle}>
        {t('aiDraft.title')}
      </h2>
      {read.isPending && <Loading />}
      {read.isError && (
        <QueryError
          error={read.error}
          onRetry={() => {
            void read.refetch();
          }}
        />
      )}
      {read.data !== undefined && (
        <DraftBody project={project} sourceBranch={sourceBranch} data={read.data} waiting={waiting} />
      )}
    </section>
  );
}

function DraftBody({
  project,
  sourceBranch,
  data,
  waiting,
}: {
  project: string;
  sourceBranch: string;
  data: AiDraft;
  waiting: boolean;
}) {
  const { t } = useTranslation();
  return (
    <div className={styles.aiAdvice}>
      <p className={styles.aiAdviceLabel}>
        <Glyph mark={'✎'} />
        {t('aiDraft.label')}
      </p>
      {data.draft === null && data.state === 'unrecorded' && data.branch !== null ? (
        // A draft branch without its record: never said as "no draft".
        <p className={styles.warning} data-testid="ai-draft-unrecorded">
          {t('aiDraft.unrecorded')}{' '}
          <Link to={treeUrl(project, data.branch)}>
            <code>{data.branch}</code>
          </Link>
        </p>
      ) : data.draft === null ? (
        // A closed or ready merge request gets no draft any more: no promise.
        <p data-testid="ai-draft-none">{waiting ? t('aiDraft.none') : t('aiDraft.noneDone')}</p>
      ) : (
        <Proposed project={project} sourceBranch={sourceBranch} draft={data.draft} />
      )}
      <p className={styles.aiAdviceMeta} data-testid="ai-draft-only">
        {t('aiDraft.suggestionOnly')}
      </p>
    </div>
  );
}

function Proposed({ project, sourceBranch, draft }: { project: string; sourceBranch: string; draft: AiDraftDetail }) {
  const { t, i18n } = useTranslation();
  const present = draft.branch_state !== 'deleted';
  return (
    <>
      <dl className={styles.aiDraftFacts}>
        <dt>{t('aiDraft.branch')}</dt>
        <dd>
          {present ? (
            <Link to={treeUrl(project, draft.branch)} data-testid="ai-draft-branch">
              <code>{draft.branch}</code>
            </Link>
          ) : (
            <code data-testid="ai-draft-branch">{draft.branch}</code>
          )}
        </dd>
        <dt>{t('aiDraft.commit')}</dt>
        <dd>
          <Link to={commitUrl(project, draft.commit_sha)} data-testid="ai-draft-commit">
            <code>{draft.commit_sha.slice(0, SHORT_SHA)}</code>
          </Link>
        </dd>
      </dl>
      {present && (
        <p className={styles.nextHint} data-testid="ai-draft-next">
          {t('aiDraft.next', { source: sourceBranch })}
        </p>
      )}
      {draft.branch_state === 'moved' && (
        <p className={styles.warning} data-testid="ai-draft-moved">
          {t('aiDraft.moved', {
            head: (draft.branch_head ?? '').slice(0, SHORT_SHA),
          })}
        </p>
      )}
      {draft.branch_state === 'deleted' && (
        <p className={styles.warning} data-testid="ai-draft-deleted">
          {t('aiDraft.deleted')}
        </p>
      )}
      <h3 className={styles.aiAdviceHeading}>{t('aiDraft.filesTitle', { n: draft.files.length })}</h3>
      {draft.files.length === 0 ? (
        <p>{t('aiDraft.noFiles')}</p>
      ) : (
        <ul className={styles.aiDraftFiles} data-testid="ai-draft-files">
          {draft.files.map((path) => (
            <li key={path}>
              {present ? (
                <Link to={blobUrl(project, draft.branch, path)}>
                  <code>{path}</code>
                </Link>
              ) : (
                <code>{path}</code>
              )}
            </li>
          ))}
        </ul>
      )}
      <p data-testid="ai-draft-dropped">
        {draft.dropped_edits > 0 ? t('aiDraft.dropped', { n: draft.dropped_edits }) : t('aiDraft.noDropped')}
      </p>
      <p className={styles.aiAdviceMeta}>
        <time dateTime={draft.proposed_at}>
          {t('aiDraft.made', {
            at: formatDateTime(draft.proposed_at, i18n.language),
          })}
        </time>
      </p>
    </>
  );
}
