// `/-/confirm/projects/{id}/merge-requests/{iid}?intent&sha&integration&state`
// (U3, ADR-0025 §5.4): the popup where a person confirms that an app (kanflow)
// may approve / unapprove a merge request for them. It shows forge's data only
// (never text the app sent), and the press writes a pending assertion; the
// app then claims it. Defenses: served unframable (headers in forge-web), the
// button works only after a visible, focused second plus a fresh pointer / key
// action (`useArmed`), one press at a time, no default focus, no form (Enter
// submits nothing by itself).
import { useQuery } from '@tanstack/react-query';
import { useEffect, useRef, useState, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { useParams, useSearchParams } from 'react-router';

import {
  ASSERTION_TTL_MINUTES,
  takeConfirmLink,
  useActAsConfirmation,
  useIssueAssertion,
  type ActAsConfirmation,
  type ConfirmLink,
  type IssueMutation,
} from '../../api/actAs';
import { ApiFailure, call } from '../../api/client';
import { useApi } from '../../api/context';
import { parseIid } from '../../api/mergeRequests';
import { ErrorNotice } from '../../components/ErrorNotice';
import { Loading } from '../../components/Loading';
import { QueryError } from '../../components/QueryError';
import controls from '../../components/controls.module.css';
import { useDocumentTitle } from '../../hooks/useDocumentTitle';
import styles from './ActAsConfirm.module.css';
import { useArmed } from './useArmed';

/** How long this page stays usable after it opened (ms). The pending
 * assertion lives 5 minutes from the press; a page left open longer than this
 * shows a head the person saw long ago, so it asks to be opened again. */
export const PAGE_TTL_MS = 10 * 60 * 1000;
export const PAGE_TTL_MINUTES = PAGE_TTL_MS / 60000;

/** The longest merge request title shown (characters). The title is the
 * author's text, not a fact forge checked: it is shown cut short and quiet,
 * and the number and the commit are what the person compares. */
export const TITLE_MAX_CHARS = 80;

/** The user-perceived characters of `text` (grapheme clusters: an emoji
 * or a combined character is never cut in half). Without `Intl.Segmenter`
 * (an older browser) code points: never half a surrogate pair. */
export function graphemes(text: string): string[] {
  // The lib types say it always exists; an older browser has none.
  const Segmenter = (Intl as { Segmenter?: typeof Intl.Segmenter }).Segmenter;
  if (Segmenter === undefined) return Array.from(text);
  return Array.from(new Segmenter(undefined, { granularity: 'grapheme' }).segment(text), (s) => s.segment);
}

/** The bidirectional embedding, override and isolate controls (U+202A-202E,
 * U+2066-2069): an author's title must not reorder what is shown. */
const BIDI_CONTROLS = /[\u202A-\u202E\u2066-\u2069]/g;

/** `text` on one line without bidi controls, cut to at most `max`
 * characters, ending in "…" when it was cut. */
export function shortTitle(text: string, max = TITLE_MAX_CHARS): string {
  const chars = graphemes(text.replace(BIDI_CONTROLS, '').replace(/\s+/g, ' ').trim());
  return chars.length <= max ? chars.join('') : `${chars.slice(0, max).join('')}…`;
}

export type NoticeKind = 'invalidLink' | 'refused' | 'notFound' | 'bindingNotFound' | 'expired' | 'alreadyDecided' | 'stale';

export type End = { kind: 'done' } | { kind: 'canceled' } | { kind: NoticeKind; requestId: string | null };

export function ActAsConfirm() {
  const { t } = useTranslation();
  const params = useParams();
  const [search] = useSearchParams();
  const project = parseIid(params.id ?? '');
  const iid = parseIid(params.iid ?? '');
  // Read once; the state leaves the address bar and never goes to a query key
  // or a log (see `takeConfirmLink`).
  const [link] = useState(() => takeConfirmLink(search));
  const [late, setLate] = useState(false);
  const [end, setEnd] = useState<End | null>(null);
  // The press lives here, so the page timeout can see a press in flight.
  const issue = useIssueAssertion(project ?? 0, iid ?? 0);
  useDocumentTitle(t('actAs.title'));

  useEffect(() => {
    const timer = window.setTimeout(() => {
      setLate(true);
    }, PAGE_TTL_MS);
    return () => {
      window.clearTimeout(timer);
    };
  }, []);

  const current = link;
  // Past the timeout the page expires - but never while a press is in flight:
  // its answer decides (a success is "done", a failure then expires).
  const finished: End | null = issue.isSuccess
    ? { kind: 'done' }
    : (end ?? (late && !issue.isPending ? { kind: 'expired', requestId: null } : null));
  const confirmation = useActAsConfirmation(project ?? 0, iid ?? 0, current, finished === null);

  let body: ReactNode;
  if (project === null || iid === null || current === null) {
    body = <Notice kind="invalidLink" requestId={null} />;
  } else if (finished !== null) {
    body = <Ended end={finished} />;
  } else if (confirmation.isPending) {
    body = <Loading />;
  } else if (confirmation.isError) {
    const e = confirmation.error;
    const f = e instanceof ApiFailure && e.failure.kind === 'api' ? e.failure : null;
    if (f?.status === 403) {
      // The refusal says nothing more: not who may, not why (a 403 is also a
      // disabled integration, so it never blames the person). The request
      // id lets an administrator find the logged reason.
      body = <Notice kind="refused" requestId={f.requestId} />;
    } else if (f?.status === 404) {
      body = <Notice kind="notFound" requestId={f.requestId} />;
    } else {
      body = (
        <QueryError
          error={e}
          onRetry={() => {
            void confirmation.refetch();
          }}
        />
      );
    }
  } else {
    body = <Confirmation project={project} iid={iid} link={current} data={confirmation.data} issue={issue} onEnd={setEnd} />;
  }
  return (
    <div className={styles.page}>
      <h1 className={styles.title}>{t('actAs.title')}</h1>
      {body}
    </div>
  );
}

export function CloseButton() {
  const { t } = useTranslation();
  const [failed, setFailed] = useState(false);
  return (
    <>
      <button
        type="button"
        className={controls.button}
        onClick={() => {
          window.close();
          // A page that no script opened cannot close itself.
          setFailed(true);
        }}
      >
        {t('actAs.close')}
      </button>
      {failed && <p className={styles.small}>{t('actAs.closeByHand')}</p>}
    </>
  );
}

export function Notice({ kind, requestId }: { kind: NoticeKind; requestId: string | null }) {
  const { t } = useTranslation();
  const text = {
    invalidLink: [t('actAs.invalidTitle'), t('actAs.invalidBody')],
    refused: [t('actAs.refusedTitle'), t('actAs.refusedBody')],
    notFound: [t('actAs.notFoundTitle'), t('actAs.notFoundBody')],
    bindingNotFound: [t('actAs.notFoundTitle'), t('actAs.binding.notFoundBody')],
    expired: [t('actAs.expiredTitle'), t('actAs.expiredBody')],
    alreadyDecided: [t('actAs.alreadyTitle'), t('actAs.alreadyBody')],
    stale: [t('actAs.staleTitle'), t('actAs.staleBody')],
  }[kind];
  return (
    <section role="alert" className={styles.notice} aria-labelledby={`act-as-${kind}`}>
      <h2 id={`act-as-${kind}`} className={styles.noticeTitle}>
        {text[0]}
      </h2>
      <p>{text[1]}</p>
      {requestId !== null && (
        <p className={styles.small}>
          {t('error.requestId')}
          {': '}
          <code data-testid="request-id">{requestId}</code>
        </p>
      )}
      <CloseButton />
    </section>
  );
}

export function Ended({ end }: { end: End }) {
  const { t } = useTranslation();
  if (end.kind === 'done' || end.kind === 'canceled') {
    const done = end.kind === 'done';
    return (
      <section role="status" className={done ? styles.done : styles.canceled}>
        <h2 className={styles.noticeTitle}>{done ? t('actAs.doneTitle') : t('actAs.canceledTitle')}</h2>
        <p>{done ? t('actAs.doneBody') : t('actAs.canceledBody')}</p>
        <CloseButton />
      </section>
    );
  }
  return <Notice kind={end.kind} requestId={end.requestId} />;
}

export function Row({ label, children }: { label: string; children: ReactNode }) {
  return (
    <>
      <dt>{label}</dt>
      <dd>{children}</dd>
    </>
  );
}

function Confirmation({
  project,
  iid,
  link,
  data,
  issue,
  onEnd,
}: {
  project: number;
  iid: number;
  link: ConfirmLink;
  data: ActAsConfirmation;
  issue: IssueMutation;
  onEnd: (e: End) => void;
}) {
  const { t } = useTranslation();
  const api = useApi();
  const button = useRef<HTMLButtonElement>(null);
  const { armed, waited, take } = useArmed(button);
  const [tooSoon, setTooSoon] = useState(false);
  const projectInfo = useQuery({
    queryKey: ['project', project],
    queryFn: ({ signal }) =>
      call(() => api.GET('/api/v1/projects/{id}', { signal, params: { path: { id: project } } })),
  });
  const approve = link.intent === 'approve';
  const app = `${data.integration_kind} (${data.integration_name})`;
  const person = `${data.person_display_name} (${data.person_username})`;

  // The head the app showed is not the head now: nothing to press.
  if (!data.sha_matches || data.head_sha === null || data.target_sha === null) {
    return <Notice kind="stale" requestId={null} />;
  }
  const targetSha = data.target_sha;

  const press = () => {
    // One press at a time (the mutation is the guard), and only after an
    // arming action older than the gap.
    if (issue.isPending) return;
    const accepted = take();
    setTooSoon(!accepted && armed);
    if (!accepted) return;
    issue.mutate(
      { link, targetSha },
      {
        onError: (e) => {
          if (!(e instanceof ApiFailure) || e.failure.kind !== 'api') return;
          const requestId = e.failure.requestId;
          // 409: the head or target moved. 400: used before or not valid.
          if (e.failure.status === 409) onEnd({ kind: 'stale', requestId });
          else if (e.failure.status === 400) onEnd({ kind: 'alreadyDecided', requestId });
          else if (e.failure.status === 404) onEnd({ kind: 'notFound', requestId });
        },
      },
    );
  };

  const ready = armed && !issue.isPending;
  const status = issue.isPending
    ? t('actAs.sending')
    : tooSoon
      ? t('actAs.tooSoon')
      : armed
      ? t('actAs.ready')
      : waited
        ? t('actAs.moveToArm')
        : t('actAs.waiting');
  return (
    <>
      <p className={styles.lead}>{approve ? t('actAs.leadApprove', { app }) : t('actAs.leadUnapprove', { app })}</p>
      <p className={styles.hint}>{approve ? t('actAs.pressApprove') : t('actAs.pressUnapprove')}</p>
      <dl className={styles.summary}>
        <Row label={t('actAs.rowApp')}>{app}</Row>
        <Row label={t('actAs.rowPerson')}>{person}</Row>
        <Row label={t('actAs.rowAction')}>
          <strong>{approve ? t('actAs.actionApprove') : t('actAs.actionUnapprove')}</strong>
        </Row>
        <Row label={t('actAs.rowProject')}>
          {projectInfo.isSuccess ? projectInfo.data.path : t('actAs.projectN', { id: project })}
        </Row>
        <Row label={t('actAs.rowMr')}>
          <strong className={styles.fact} data-testid="mr-iid">
            {t('actAs.mrN', { iid })}
          </strong>{' '}
          <span className={styles.mrTitle} data-testid="mr-title">
            {shortTitle(data.title)}
          </span>
        </Row>
        <Row label={t('actAs.rowBranches')}>
          <code>{data.source_branch}</code> {t('actAs.into')} <code>{data.target_branch}</code>
        </Row>
        <Row label={t('actAs.rowHead')}>
          <strong className={styles.fact}>
            <code className={styles.sha}>{data.head_sha}</code>
          </strong>
        </Row>
        <Row label={t('actAs.rowTargetTip')}>
          <code className={styles.sha}>{data.target_sha}</code>
        </Row>
        <Row label={t('actAs.rowExpiry')}>{t('actAs.expiry', { minutes: PAGE_TTL_MINUTES, assertion: ASSERTION_TTL_MINUTES })}</Row>
      </dl>
      {issue.isError &&
        (issue.error instanceof ApiFailure ? (
          issue.error.failure.kind === 'unauthenticated' ? (
            <QueryError error={issue.error} />
          ) : (
            <ErrorNotice failure={issue.error.failure} />
          )
        ) : (
          <ErrorNotice failure={{ kind: 'unexpected', status: null, requestId: null }} />
        ))}
      <div className={styles.buttons}>
        <button
          type="button"
          ref={button}
          className={controls.primary}
          aria-disabled={!ready}
          aria-describedby="act-as-status"
          onClick={press}
        >
          {approve ? t('actAs.confirmApprove') : t('actAs.confirmUnapprove')}
        </button>
        <button
          type="button"
          className={controls.button}
          disabled={issue.isPending}
          onClick={() => {
            onEnd({ kind: 'canceled' });
          }}
        >
          {t('actAs.cancel')}
        </button>
      </div>
      <p id="act-as-status" role="status" className={styles.small}>
        {status}
      </p>
    </>
  );
}
