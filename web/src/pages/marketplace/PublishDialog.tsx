// The publish dialog (plan 16 A8, §6): publish the tag <slug>/v<version> of
// the parts repository. Before: the capabilities the newest version
// declares now. After: the CI result the server checked (the publication is
// refused unless the commit's newest pipeline succeeded, FG-AGENT-0004) and
// what the new version adds or drops against the previous one. A refusal
// shows the server's user_message and request id.
import { useQuery } from '@tanstack/react-query';
import { useEffect, useRef, useState, type SyntheticEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { Link } from 'react-router';

import { useApi } from '../../api/context';
import { capabilityDiff, usePublish, type PartSummary, type PartVersionDetail } from '../../api/marketplace';
import { projectQuery } from '../../api/namespaces';
import { TextField } from '../../components/Field';
import controls from '../../components/controls.module.css';
import { ErrorSummary } from '../../features/CreateDialogs';
import { CapabilityMarks } from '../../features/marketplace/marks';
import { pipelineUrl } from '../../features/repoUrls';
import { Modal } from '../../ui/Modal';
import { Denied } from '../settings/common';
import styles from './Marketplace.module.css';

const SLUG = /^[a-z0-9][a-z0-9-]{0,63}$/;
const VERSION = /^\d+\.\d+\.\d+$/;

export function PublishDialog({
  open,
  parts,
  onClose,
}: {
  open: boolean;
  parts: readonly PartSummary[];
  onClose: () => void;
}) {
  const { t } = useTranslation();
  const publish = usePublish();
  return (
    <Modal
      open={open}
      busy={publish.isPending}
      title={t('market.publish.title')}
      onClose={() => {
        publish.reset();
        onClose();
      }}
    >
      <PublishForm parts={parts} publish={publish} />
    </Modal>
  );
}

function PublishForm({ parts, publish }: { parts: readonly PartSummary[]; publish: ReturnType<typeof usePublish> }) {
  const { t } = useTranslation();
  const [slug, setSlug] = useState('');
  const [version, setVersion] = useState('');
  const [attempt, setAttempt] = useState(0);
  const [before, setBefore] = useState<{ version: string; caps: string[] } | null>(null);
  const s = slug.trim();
  const v = version.trim();
  const current = parts.find((p) => p.slug === s)?.latest ?? null;
  const slugError = s === '' ? t('market.publish.slugEmpty') : SLUG.test(s) ? null : t('market.publish.slugInvalid');
  const versionError =
    v === '' ? t('market.publish.versionEmpty') : VERSION.test(v) ? null : t('market.publish.versionInvalid');
  const errors = [slugError, versionError].filter((e): e is string => e !== null);
  const submit = (e: SyntheticEvent) => {
    e.preventDefault();
    if (publish.isPending) return;
    setAttempt((n) => n + 1);
    if (errors.length > 0) return;
    setBefore(current === null ? null : { version: current.version, caps: current.capabilities });
    publish.mutate({ slug: s, tag: `${s}/v${v}` });
  };
  if (publish.isSuccess) return <Published result={publish.data} before={before} />;
  return (
    <form onSubmit={submit} noValidate className={styles.dialogForm}>
      <p className={styles.hint}>{t('market.publish.explain')}</p>
      <ErrorSummary errors={errors} attempt={attempt} />
      <TextField
        label={t('market.publish.slug')}
        value={slug}
        onChange={setSlug}
        error={attempt > 0 ? slugError : null}
        help={t('market.publish.slugHelp')}
        required
        initialFocus
      />
      <TextField
        label={t('market.publish.version')}
        value={version}
        onChange={setVersion}
        error={attempt > 0 ? versionError : null}
        help={t('market.publish.versionHelp')}
        required
      />
      {s !== '' && v !== '' && errors.length === 0 && (
        <p className={styles.hint}>{t('market.publish.tag', { tag: `${s}/v${v}` })}</p>
      )}
      {s !== '' &&
        (current === null ? (
          <p className={styles.hint}>{t('market.publish.newPart')}</p>
        ) : (
          <div className={styles.block}>
            <p className={styles.blockTitle}>{t('market.publish.currentCaps', { version: current.version })}</p>
            <CapabilityMarks caps={current.capabilities} />
          </div>
        ))}
      {publish.isError && <Denied error={publish.error} />}
      <div className={styles.actions}>
        <button type="submit" className={controls.primary} aria-disabled={publish.isPending ? 'true' : undefined}>
          {publish.isPending ? t('market.publish.busy') : t('market.publish.submit')}
        </button>
      </div>
      <p className={styles.hint}>{t('market.publish.after')}</p>
    </form>
  );
}

function Published({ result, before }: { result: PartVersionDetail; before: { version: string; caps: string[] } | null }) {
  const { t } = useTranslation();
  const api = useApi();
  const v = result.version;
  const project = useQuery(projectQuery(api, v.source_project_id));
  const diff = capabilityDiff(before?.caps ?? [], v.capabilities);
  const ci = t('market.publish.ci', { id: v.ci_pipeline_id });
  // The form (and its focused button) is gone: the result takes the focus.
  const title = useRef<HTMLParagraphElement>(null);
  useEffect(() => {
    title.current?.focus();
  }, []);
  return (
    <div className={styles.dialogForm}>
      <div role="status" className={styles.done}>
        <p className={styles.doneTitle} ref={title} tabIndex={-1}>
          {t('market.publish.done', { version: v.version, channel: v.channel })}
        </p>
        <p>
          {project.isSuccess ? <Link to={pipelineUrl(project.data.path, v.ci_pipeline_id)}>{ci}</Link> : ci}
        </p>
      </div>
      <div className={styles.block}>
        <p className={styles.blockTitle}>
          {before === null
            ? t('market.publish.diffFirst')
            : t('market.publish.diffAgainst', { version: before.version })}
        </p>
        <h3 className={styles.diffTitle}>{t('market.publish.added')}</h3>
        <CapabilityMarks caps={diff.added} label={t('market.publish.added')} none={t('market.publish.noChange')} />
        <h3 className={styles.diffTitle}>{t('market.publish.removed')}</h3>
        <CapabilityMarks caps={diff.removed} label={t('market.publish.removed')} none={t('market.publish.noChange')} />
      </div>
      <p className={styles.hint}>{t('market.publish.next')}</p>
    </div>
  );
}
