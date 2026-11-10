// `/-/confirm/projects/{id}/integration-bindings?intent&integration&kanflow_project_id[&resolves_work_items]&state`
// (M3 X3, ADR-0025 Amendment X3 §4): the popup where a person confirms that
// an app (kanflow) may bind a repository to one of its projects — or remove
// that binding — for them. The same defenses as the merge request popup
// (`ActAsConfirm`): forge's data only (the kanflow project is shown as the id
// the app named, never text it sent), served unframable, the button works only
// after a visible, focused second plus a fresh action (`useArmed`), one press
// at a time, no default focus, no form.
import { useEffect, useRef, useState, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { useParams, useSearchParams } from 'react-router';

import {
  ASSERTION_TTL_MINUTES,
  takeBindingLink,
  useBindingConfirmation,
  useIssueBindingAssertion,
  type BindingConfirmation,
  type BindingLink,
  type IssueBindingMutation,
} from '../../api/actAs';
import { ApiFailure } from '../../api/client';
import { parseIid } from '../../api/mergeRequests';
import { ErrorNotice } from '../../components/ErrorNotice';
import { Loading } from '../../components/Loading';
import { QueryError } from '../../components/QueryError';
import controls from '../../components/controls.module.css';
import { useDocumentTitle } from '../../hooks/useDocumentTitle';
import { Ended, Notice, PAGE_TTL_MINUTES, PAGE_TTL_MS, Row, type End } from './ActAsConfirm';
import styles from './ActAsConfirm.module.css';
import { useArmed } from './useArmed';

export function ActAsBindingConfirm() {
  const { t } = useTranslation();
  const params = useParams();
  const [search] = useSearchParams();
  const project = parseIid(params.id ?? '');
  // Read once; the state leaves the address bar (see `takeBindingLink`).
  const [link] = useState(() => takeBindingLink(search));
  const [late, setLate] = useState(false);
  const [end, setEnd] = useState<End | null>(null);
  const issue = useIssueBindingAssertion(project ?? 0);
  useDocumentTitle(t('actAs.title'));

  useEffect(() => {
    const timer = window.setTimeout(() => {
      setLate(true);
    }, PAGE_TTL_MS);
    return () => {
      window.clearTimeout(timer);
    };
  }, []);

  // Past the timeout the page expires - never while a press is in flight.
  const finished: End | null = issue.isSuccess
    ? { kind: 'done' }
    : (end ?? (late && !issue.isPending ? { kind: 'expired', requestId: null } : null));
  const confirmation = useBindingConfirmation(project ?? 0, link, finished === null);

  let body: ReactNode;
  if (project === null || link === null) {
    body = <Notice kind="invalidLink" requestId={null} />;
  } else if (finished !== null) {
    body = <Ended end={finished} />;
  } else if (confirmation.isPending) {
    body = <Loading />;
  } else if (confirmation.isError) {
    const e = confirmation.error;
    const f = e instanceof ApiFailure && e.failure.kind === 'api' ? e.failure : null;
    if (f?.status === 403) {
      // Neutral (what is left as a 403 is the server's set-up, e.g. acting
      // for people not configured), with the request id.
      body = <Notice kind="refused" requestId={f.requestId} />;
    } else if (f?.status === 404) {
      // The same neutral notice as the merge request's screen (ADR-0025,
      // decision of 2026-11-10: every refusal is the invisible project's 404).
      body = <Notice kind="notFound" requestId={f.requestId} />;
    } else if (f?.status === 400) {
      body = <Notice kind="invalidLink" requestId={f.requestId} />;
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
    body = <Confirmation link={link} data={confirmation.data} issue={issue} onEnd={setEnd} />;
  }
  return (
    <div className={styles.page}>
      <h1 className={styles.title}>{t('actAs.title')}</h1>
      {body}
    </div>
  );
}

function Confirmation({
  link,
  data,
  issue,
  onEnd,
}: {
  link: BindingLink;
  data: BindingConfirmation;
  issue: IssueBindingMutation;
  onEnd: (e: End) => void;
}) {
  const { t } = useTranslation();
  const button = useRef<HTMLButtonElement>(null);
  const { armed, waited, take } = useArmed(button);
  const [tooSoon, setTooSoon] = useState(false);
  const set = link.intent === 'binding_set';
  const app = `${data.integration_kind} (${data.integration_name})`;
  const person = `${data.person_display_name} (${data.person_username})`;

  // Removing a binding that is not there: nothing to press.
  if (!set && !data.bound) {
    return <Notice kind="alreadyDecided" requestId={null} />;
  }

  const press = () => {
    if (issue.isPending) return;
    const accepted = take();
    setTooSoon(!accepted && armed);
    if (!accepted) return;
    issue.mutate(link, {
      onError: (e) => {
        if (!(e instanceof ApiFailure) || e.failure.kind !== 'api') return;
        const requestId = e.failure.requestId;
        if (e.failure.status === 409) onEnd({ kind: 'stale', requestId });
        else if (e.failure.status === 400) onEnd({ kind: 'alreadyDecided', requestId });
        else if (e.failure.status === 404) onEnd({ kind: 'notFound', requestId });
      },
    });
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
      <p className={styles.lead}>{set ? t('actAs.binding.leadSet', { app }) : t('actAs.binding.leadRemove', { app })}</p>
      <p className={styles.hint}>{set ? t('actAs.binding.pressSet') : t('actAs.binding.pressRemove')}</p>
      <dl className={styles.summary}>
        <Row label={t('actAs.rowApp')}>{app}</Row>
        <Row label={t('actAs.rowPerson')}>{person}</Row>
        <Row label={t('actAs.rowAction')}>
          <strong>{set ? t('actAs.binding.actionSet') : t('actAs.binding.actionRemove')}</strong>
        </Row>
        <Row label={t('actAs.binding.rowRepository')}>
          <code>{data.project_path}</code>
        </Row>
        <Row label={t('actAs.binding.rowKanflowProject')}>
          <code>{data.kanflow_project_id}</code>
        </Row>
        {data.resolves_work_items !== null && (
          <Row label={t('actAs.binding.rowSetting')}>
            {data.resolves_work_items ? t('actAs.binding.settingCode') : t('actAs.binding.settingShared')}
          </Row>
        )}
        <Row label={t('actAs.binding.rowNow')}>{data.bound ? t('actAs.binding.nowBound') : t('actAs.binding.nowUnbound')}</Row>
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
          aria-describedby="act-as-binding-status"
          onClick={press}
        >
          {set ? t('actAs.binding.confirmSet') : t('actAs.binding.confirmRemove')}
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
      <p id="act-as-binding-status" role="status" className={styles.small}>
        {status}
      </p>
    </>
  );
}
