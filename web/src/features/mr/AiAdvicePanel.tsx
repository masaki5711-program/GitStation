// The local LLM's first pass on the merge request screen (plan 19 §3.1 /
// §4, L5-6; ADR-0039 amendments L5-5 and L5-6), as advice and nothing else:
// - labelled 「AI の助言（提案）」 in words and a glyph (never the colour
//   alone), framed apart from the people's threads, with the model, the
//   prompt version and when it was made;
// - the deep-review mark first (its reasons as the server orders them: the
//   code rules, then the model's words), then the summary, then the findings
//   with the file, the line and the evidence quoted from the change;
// - an advice made for another commit says so and names both commits; a
//   less trustworthy one says so; a confidential one read by an agent is
//   withheld (the server's decision); none says "no advice now";
// - a failed first pass says it failed (never shown as a pass).
// Every word of the advice is plain text (never HTML, never Markdown). The
// only controls are 「役に立った / 誤り」 per finding, drawn only when the
// server says the reader may vote (`can_feedback`); nothing here approves,
// merges or blocks anything. Shown only when the project uses the first
// pass or something was made (as the autofix and auto-merge lines).
import type { TFunction } from 'i18next';
import { useEffect, useRef } from 'react';
import { useTranslation } from 'react-i18next';

import {
  aiAdviceVisible,
  myVerdict,
  useAiFeedback,
  useAiReview,
  type AiReview,
  type AiReviewAdvice,
  type AiReviewFinding,
  type AiVerdict,
} from '../../api/aiReview';
import controls from '../../components/controls.module.css';
import { Loading } from '../../components/Loading';
import { QueryError } from '../../components/QueryError';
import { formatDateTime } from '../labels';
import { useAnnounce } from './announce';
import styles from './Mr.module.css';

const TITLE_ID = 'mr-ai-advice-title';
/** Commits are named by their first characters (as elsewhere on the screen). */
const SHORT_SHA = 8;

export function severityLabel(t: TFunction, severity: string): string {
  switch (severity) {
    case 'critical':
      return t('aiAdvice.severity.critical');
    case 'high':
      return t('aiAdvice.severity.high');
    case 'medium':
      return t('aiAdvice.severity.medium');
    default:
      return t('aiAdvice.severity.low');
  }
}

function verdictLabel(t: TFunction, verdict: AiVerdict): string {
  return verdict === 'useful' ? t('aiAdvice.useful') : t('aiAdvice.wrong');
}

/** What the first pass is doing, as one word: a change of it after the
 * first answer is said once (the review finishes, fails or a new advice
 * arrives while the screen is open). */
export type AiPhase = 'running' | 'failed' | 'none' | 'shown' | 'withheld';

export function aiPhase(data: AiReview): AiPhase {
  if (data.check !== null && data.check.status !== 'completed') return 'running';
  if (data.check?.status === 'completed' && data.check.conclusion === 'error') return 'failed';
  if (data.advice.state === 'shown') return 'shown';
  if (data.advice.state === 'withheld') return 'withheld';
  return 'none';
}

function phaseText(t: TFunction, phase: AiPhase): string {
  switch (phase) {
    case 'running':
      return t('aiAdvice.running');
    case 'failed':
      return t('aiAdvice.failed');
    case 'shown':
      return t('aiAdvice.arrived');
    case 'withheld':
      return t('aiAdvice.withheld');
    case 'none':
      return t('aiAdvice.none');
  }
}

/** A glyph for the eye only: the words beside it carry the meaning. */
const Glyph = ({ mark }: { mark: string }) => <span aria-hidden="true">{mark} </span>;

export function AiAdviceSection({ projectId, iid, open }: { projectId: number; iid: number; open: boolean }) {
  const { t } = useTranslation();
  const read = useAiReview(projectId, iid, open);
  const announce = useAnnounce();
  const phase = read.data === undefined ? null : aiPhase(read.data);
  const last = useRef<AiPhase | null>(null);
  useEffect(() => {
    if (phase === null) return;
    // The first answer is the page's own content, not a change.
    if (last.current !== null && last.current !== phase) announce(phaseText(t, phase));
    last.current = phase;
  }, [phase, announce, t]);
  if (read.isSuccess && !aiAdviceVisible(read.data)) return null;
  return (
    <section
      aria-labelledby={TITLE_ID}
      className={styles.detailSection}
      data-testid="mr-ai-advice"
      data-state={read.data?.advice.state ?? (read.isError ? 'error' : 'loading')}
    >
      <h2 id={TITLE_ID} className={styles.detailTitle}>
        {t('aiAdvice.title')}
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
      {read.data !== undefined && <AdviceBody projectId={projectId} iid={iid} data={read.data} />}
    </section>
  );
}

function AdviceBody({ projectId, iid, data }: { projectId: number; iid: number; data: AiReview }) {
  const { t, i18n } = useTranslation();
  const advice = data.advice;
  const running = data.check !== null && data.check.status !== 'completed';
  const failed = data.check?.status === 'completed' && data.check.conclusion === 'error';
  return (
    <div className={styles.aiAdvice}>
      <p className={styles.aiAdviceLabel}>
        <Glyph mark={'\u2726'} />
        {t('aiAdvice.label')}
      </p>
      {running && (
        <p className={styles.nextHint} data-testid="ai-advice-running">
          {t('aiAdvice.running')}
        </p>
      )}
      {failed && (
        <p className={styles.warning} data-testid="ai-advice-failed">
          {t('aiAdvice.failed')}
        </p>
      )}
      {advice.state === 'none' && <p data-testid="ai-advice-none">{t('aiAdvice.none')}</p>}
      {advice.state === 'withheld' && <p data-testid="ai-advice-withheld">{t('aiAdvice.withheld')}</p>}
      {advice.state === 'shown' && (
        <Shown projectId={projectId} iid={iid} advice={advice} head={data.head_sha} canVote={data.can_feedback} />
      )}
      {advice.state !== 'none' && (
        <p className={styles.aiAdviceMeta}>
          {advice.model !== null && <span>{t('deploy.advice.model', { model: advice.model })}</span>}
          {advice.prompt_version !== null && (
            <span>{t('deploy.advice.prompt', { version: advice.prompt_version })}</span>
          )}
          {advice.created_at !== null && (
            <time dateTime={advice.created_at}>
              {t('deploy.advice.made', { at: formatDateTime(advice.created_at, i18n.language) })}
            </time>
          )}
        </p>
      )}
      <p className={styles.aiAdviceMeta} data-testid="ai-advice-only">
        {t('aiAdvice.adviceOnly')}
      </p>
    </div>
  );
}

function Shown({
  projectId,
  iid,
  advice,
  head,
  canVote,
}: {
  projectId: number;
  iid: number;
  advice: AiReviewAdvice;
  head: string | null;
  canVote: boolean;
}) {
  const { t } = useTranslation();
  const vote = useAiFeedback(projectId, iid);
  const announce = useAnnounce();
  const adviceId = advice.advice_id;
  const send = (findingIndex: number, verdict: AiVerdict) => {
    if (adviceId === null) return;
    vote.mutate(
      { adviceId, findingIndex, verdict },
      {
        onSuccess: (answer) => {
          announce(t('aiAdvice.recorded', { verdict: verdictLabel(t, answer.verdict === 'wrong' ? 'wrong' : 'useful') }));
        },
      },
    );
  };
  return (
    <>
      {advice.stale && (
        <p className={styles.warning} data-testid="ai-advice-stale">
          {t('aiAdvice.stale', {
            sha: (advice.head_sha ?? '').slice(0, SHORT_SHA) || t('aiAdvice.unknownCommit'),
            head: (head ?? '').slice(0, SHORT_SHA) || t('aiAdvice.unknownCommit'),
          })}
        </p>
      )}
      {advice.degraded === true && (
        <p className={styles.warning} data-testid="ai-advice-degraded">
          {t('aiAdvice.degraded')}
        </p>
      )}
      {advice.deep_review !== null && (
        <div className={styles.aiAdviceDeep} data-testid="ai-advice-deep">
          <p className={styles.aiAdviceDeepTitle}>
            <Glyph mark={'\u2691'} />
            {t('aiAdvice.deepTitle')}
          </p>
          <p className={styles.aiAdviceText}>{advice.deep_review}</p>
        </div>
      )}
      <h3 className={styles.aiAdviceHeading}>{t('aiAdvice.summaryTitle')}</h3>
      <p className={styles.aiAdviceText} data-testid="ai-advice-summary">
        {advice.summary}
      </p>
      <h3 className={styles.aiAdviceHeading}>{t('aiAdvice.findingsTitle', { n: advice.findings.length })}</h3>
      {advice.findings.length === 0 ? (
        <p>{t('aiAdvice.noFindings')}</p>
      ) : (
        <ol className={styles.aiFindings}>
          {advice.findings.map((f, i) => (
            <Finding
              key={i}
              index={i}
              finding={f}
              verdict={myVerdict(advice, i)}
              canVote={canVote && adviceId !== null}
              pending={vote.isPending && vote.variables.findingIndex === i}
              onVote={(verdict) => {
                send(i, verdict);
              }}
            />
          ))}
        </ol>
      )}
      {vote.isError && (
        <div data-testid="ai-advice-vote-error">
          <QueryError error={vote.error} />
        </div>
      )}
    </>
  );
}

function Finding({
  index,
  finding,
  verdict,
  canVote,
  pending,
  onVote,
}: {
  index: number;
  finding: AiReviewFinding;
  verdict: AiVerdict | null;
  canVote: boolean;
  pending: boolean;
  onVote: (verdict: AiVerdict) => void;
}) {
  const { t } = useTranslation();
  const place =
    finding.path === null
      ? null
      : finding.line === null
        ? finding.path
        : t('aiAdvice.place', { path: finding.path, line: finding.line });
  const n = index + 1;
  const button = (v: AiVerdict) => (
    <button
      type="button"
      className={`${controls.button} ${controls.small} ${styles.aiVote}`}
      aria-pressed={verdict === v}
      aria-disabled={pending}
      // The finding's number in the name: every pair reads apart in a list
      // of buttons (the visible word comes first, WCAG 2.5.3).
      aria-label={v === 'useful' ? t('aiAdvice.usefulFor', { n }) : t('aiAdvice.wrongFor', { n })}
      onClick={() => {
        if (!pending) onVote(v);
      }}
    >
      {verdict === v && <Glyph mark={'\u2713'} />}
      {verdictLabel(t, v)}
    </button>
  );
  return (
    <li className={styles.aiFinding} data-testid="ai-advice-finding">
      <p className={styles.aiFindingHead}>
        <span className={styles.aiSeverity} data-severity={finding.severity}>
          {severityLabel(t, finding.severity)}
        </span>
        {place !== null && <code className={styles.aiPlace}>{place}</code>}
        <span className={styles.aiConfidence}>
          {t('aiAdvice.confidence', { pct: Math.round(finding.confidence * 100) })}
        </span>
      </p>
      <p className={styles.aiAdviceText}>{finding.message}</p>
      <p className={styles.aiEvidence}>
        <span>{t('aiAdvice.evidence')}</span>{' '}
        <code className={styles.aiQuote} data-testid="ai-advice-quote">
          {finding.quote}
        </code>
      </p>
      {canVote && (
        <div role="group" aria-label={t('aiAdvice.voteGroup', { n })} className={styles.aiVotes}>
          {button('useful')}
          {button('wrong')}
        </div>
      )}
    </li>
  );
}
