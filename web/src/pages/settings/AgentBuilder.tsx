// Project settings → "Agent" (/{project}/-/settings/agent; plan 16 A8, §6):
// build the project's Claude Code configuration (.claude/, CLAUDE.md,
// .mcp.json) from signed marketplace parts.
//
// - The update band: parts of the current lock (.gitstation/agent.lock.json
//   on the default branch) with a newer stable version recommended.
// - The adapter (the base model, exactly one), the parts as checkboxes —
//   the rules' recommendation checked, with its reason in words.
// - "See the Dry-Run diff": the files added / modified / removed, each
//   part's capability marks, a line for every part that widens permissions.
// - "Create the merge request" after a Dry-Run of the same choice; the
//   merge request waits for a Maintainer (never auto-merged).
// - The project's recent builds with their request ids.
//
// Who sees what comes from the project role the API exposes (forge-auth
// AgentBuilder*): a Reporter reads the recommendation; the Dry-Run, the
// builds and the merge request are a Developer's. The server decides again.
import { useQuery } from '@tanstack/react-query';
import type { TFunction } from 'i18next';
import { useEffect, useId, useMemo, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Link } from 'react-router';

import { useApi } from '../../api/context';
import {
  LOCK_PATH,
  lockedVersions,
  mayBuild,
  mayRecommend,
  updatesOf,
  useBuilderStatus,
  useCompile,
  useParts,
  usePreview,
  useRecommendation,
  type BuildCompiled,
  type BuildPreview,
  type BuildRecord,
  type PartSummary,
  type Recommendation,
  type SelectionItem,
} from '../../api/marketplace';
import { projectQuery } from '../../api/namespaces';
import { useBlob, useRefs } from '../../api/repo';
import controls from '../../components/controls.module.css';
import { Loading } from '../../components/Loading';
import { QueryError } from '../../components/QueryError';
import { SkeletonList } from '../../components/Skeleton';
import { formatDateTime } from '../../features/labels';
import { CapabilityMarks, ChannelBadge, kindLabel } from '../../features/marketplace/marks';
import { encodePath, pullUrl } from '../../features/repoUrls';
import { useDocumentTitle } from '../../hooks/useDocumentTitle';
import { useSession } from '../../session/session';
import { PageHeader } from '../../ui/PageHeader';
import styles from './AgentBuilder.module.css';
import { Denied } from './common';
import settings from './Settings.module.css';

/** One choosable part version. */
interface Choice {
  slug: string;
  kind: string;
  version: string;
  channel: string | null;
  capabilities: string[];
  /** The rules' reason (`base_model`, `current_lock`, `required_by=<slug>`); `null`: not recommended. */
  reason: string | null;
}

/** The reason of a recommendation in words (literal keys). */
function reasonText(t: TFunction, reason: string): string {
  if (reason === 'base_model') return t('agentBuilder.reason.baseModel');
  if (reason === 'current_lock') return t('agentBuilder.reason.currentLock');
  if (reason.startsWith('required_by=')) {
    return t('agentBuilder.reason.requiredBy', { slug: reason.slice('required_by='.length) });
  }
  return reason;
}

/** The choices: the recommendation first (its order), then every other
 * catalog part with a live version, by slug. */
export function choicesOf(rec: Recommendation | undefined, catalog: readonly PartSummary[]): Choice[] {
  const bySlug = new Map(catalog.map((p) => [p.slug, p]));
  const out: Choice[] = [];
  const seen = new Set<string>();
  for (const r of rec?.parts ?? []) {
    const c = bySlug.get(r.slug)?.latest;
    out.push({
      slug: r.slug,
      kind: r.kind,
      version: r.version,
      channel: c !== null && c !== undefined && c.version === r.version ? c.channel : null,
      capabilities: c !== null && c !== undefined && c.version === r.version ? c.capabilities : [],
      reason: r.reason,
    });
    seen.add(r.slug);
  }
  const rest = catalog
    .filter((p) => !seen.has(p.slug) && p.latest !== null && !p.deprecated)
    .sort((a, b) => a.slug.localeCompare(b.slug));
  for (const p of rest) {
    if (p.latest === null) continue;
    out.push({
      slug: p.slug,
      kind: p.kind,
      version: p.latest.version,
      channel: p.latest.channel,
      capabilities: p.latest.capabilities,
      reason: null,
    });
  }
  return out;
}

function selectionOf(choices: readonly Choice[], adapter: string | null, picked: ReadonlySet<string>): SelectionItem[] {
  const item = (c: Choice): SelectionItem =>
    c.reason === null
      ? { slug: c.slug, version: c.version, selected_by: 'user' }
      : { slug: c.slug, version: c.version, selected_by: 'rule', reason: c.reason };
  const head = choices.filter((c) => c.kind === 'adapter' && c.slug === adapter).map(item);
  const parts = choices.filter((c) => c.kind !== 'adapter' && picked.has(c.slug)).map(item);
  return [...head, ...parts];
}

export function AgentBuilderScreen({ projectId, project }: { projectId: number; project: string }) {
  const { t } = useTranslation();
  const api = useApi();
  const session = useSession();
  const info = useQuery(projectQuery(api, projectId));
  useDocumentTitle(`${t('agentBuilder.title')} · ${t('settings.title')}`);
  const back = (
    <Link to={`/${encodePath(project)}/-/settings`} className={styles.back}>
      {t('agentBuilder.back')}
    </Link>
  );
  if (info.isPending) return <Loading />;
  if (info.isError) {
    return (
      <QueryError
        error={info.error}
        onRetry={() => {
          void info.refetch();
        }}
      />
    );
  }
  const role = info.data.role;
  const isAdmin = session.user.is_admin;
  const canRead = mayRecommend(role, isAdmin);
  const canBuild = mayBuild(role, isAdmin);
  return (
    <div className={styles.page}>
      {back}
      <PageHeader title={t('agentBuilder.title')} subtitle={t('agentBuilder.subtitle')} />
      {canRead ? (
        <Builder projectId={projectId} project={project} canBuild={canBuild} />
      ) : (
        <p className={styles.hint}>{t('agentBuilder.reporterOnly')}</p>
      )}
    </div>
  );
}

function Builder({ projectId, project, canBuild }: { projectId: number; project: string; canBuild: boolean }) {
  const { t } = useTranslation();
  const rec = useRecommendation(projectId, true);
  const catalog = useParts(null, null, '');
  const refs = useRefs(projectId);
  const branch = refs.data?.default_branch ?? null;
  const lock = useBlob(projectId, branch ?? '', LOCK_PATH, branch !== null);
  const locked = lock.isSuccess && lock.data.content !== null ? lockedVersions(lock.data.content) : null;
  const updates = locked === null || !rec.isSuccess ? [] : updatesOf(locked, rec.data.parts);

  return (
    <>
      {updates.length > 0 && <UpdateBand updates={updates} canBuild={canBuild} />}
      {rec.isSuccess && rec.data.unavailable.length > 0 && (
        <p className={styles.warning}>{t('agentBuilder.unavailable', { slugs: rec.data.unavailable.join(', ') })}</p>
      )}
      {(rec.isPending || catalog.isPending) && <SkeletonList rows={4} />}
      {rec.isError && (
        <QueryError
          error={rec.error}
          onRetry={() => {
            void rec.refetch();
          }}
        />
      )}
      {catalog.isError && (
        <QueryError
          error={catalog.error}
          onRetry={() => {
            void catalog.refetch();
          }}
        />
      )}
      {rec.isSuccess && catalog.isSuccess && (
        canBuild ? (
          <Picker
            // A new recommendation starts the choice again (its defaults and reasons).
            key={JSON.stringify(rec.data.parts)}
            projectId={projectId}
            project={project}
            choices={choicesOf(rec.data, catalog.data.parts)}
          />
        ) : (
          <>
            <Recommended choices={choicesOf(rec.data, []).filter((c) => c.reason !== null)} />
            <p className={styles.hint}>{t('agentBuilder.developerOnly')}</p>
          </>
        )
      )}
      {canBuild && <History projectId={projectId} project={project} />}
    </>
  );
}

function UpdateBand({ updates, canBuild }: { updates: { slug: string; from: string; to: string }[]; canBuild: boolean }) {
  const { t } = useTranslation();
  const id = useId();
  return (
    <section className={styles.band} aria-labelledby={id}>
      <h2 id={id} className={styles.bandTitle}>
        {t('agentBuilder.update.title')}
      </h2>
      <ul className={styles.bandList}>
        {updates.map((u) => (
          <li key={u.slug}>{t('agentBuilder.update.item', { slug: u.slug, from: u.from, to: u.to })}</li>
        ))}
      </ul>
      <p className={styles.bandNext}>{canBuild ? t('agentBuilder.update.next') : t('agentBuilder.update.nextReader')}</p>
    </section>
  );
}

/** The recommendation read-only (a Reporter). */
function Recommended({ choices }: { choices: Choice[] }) {
  const { t } = useTranslation();
  return (
    <section className={styles.section} aria-labelledby="agent-recommended">
      <h2 id="agent-recommended" className={styles.sectionTitle}>
        {t('agentBuilder.recommended')}
      </h2>
      <ul className={styles.choices}>
        {choices.map((c) => (
          <li key={c.slug} className={styles.choice}>
            <span className={styles.slug}>{c.slug}</span>
            <span className={styles.meta}>{t('agentBuilder.kindVersion', { kind: kindLabel(t, c.kind), version: c.version })}</span>
            {c.reason !== null && <span className={styles.reason}>{reasonText(t, c.reason)}</span>}
          </li>
        ))}
      </ul>
    </section>
  );
}

function Picker({ projectId, project, choices }: { projectId: number; project: string; choices: Choice[] }) {
  const { t } = useTranslation();
  const adapters = choices.filter((c) => c.kind === 'adapter');
  const parts = choices.filter((c) => c.kind !== 'adapter');
  const [adapter, setAdapter] = useState<string | null>(
    () => adapters.find((a) => a.reason !== null)?.slug ?? adapters[0]?.slug ?? null,
  );
  const [picked, setPicked] = useState<Set<string>>(
    () => new Set(parts.filter((p) => p.reason !== null).map((p) => p.slug)),
  );
  const selection = useMemo(() => selectionOf(choices, adapter, picked), [choices, adapter, picked]);
  const key = JSON.stringify(selection);
  const preview = usePreview(projectId);
  const compile = useCompile(projectId);
  const [previewed, setPreviewed] = useState<{ key: string; body: BuildPreview } | null>(null);
  const [compiled, setCompiled] = useState<BuildCompiled | null>(null);
  const current = previewed !== null && previewed.key === key ? previewed.body : null;
  const legendAdapter = useId();
  const changed = () => {
    preview.reset();
    compile.reset();
    setCompiled(null);
  };

  return (
    <>
      <section className={styles.section} aria-labelledby="agent-choose">
        <h2 id="agent-choose" className={styles.sectionTitle}>
          {t('agentBuilder.choose')}
        </h2>
        <fieldset className={styles.fieldset} role="radiogroup" aria-labelledby={legendAdapter}>
          <legend id={legendAdapter} className={styles.legend}>
            {t('agentBuilder.adapter')}
          </legend>
          {adapters.length === 0 ? (
            <p className={styles.hint}>{t('agentBuilder.noAdapter')}</p>
          ) : (
            <ul className={styles.choices}>
              {adapters.map((a) => (
                <ChoiceRow
                  key={a.slug}
                  c={a}
                  type="radio"
                  checked={adapter === a.slug}
                  onChange={() => {
                    setAdapter(a.slug);
                    changed();
                  }}
                />
              ))}
            </ul>
          )}
        </fieldset>
        <fieldset className={styles.fieldset}>
          <legend className={styles.legend}>{t('agentBuilder.parts')}</legend>
          {parts.length === 0 ? (
            <p className={styles.hint}>{t('agentBuilder.noParts')}</p>
          ) : (
            <ul className={styles.choices}>
              {parts.map((p) => (
                <ChoiceRow
                  key={p.slug}
                  c={p}
                  type="checkbox"
                  checked={picked.has(p.slug)}
                  onChange={() => {
                    const next = new Set(picked);
                    if (next.has(p.slug)) next.delete(p.slug);
                    else next.add(p.slug);
                    setPicked(next);
                    changed();
                  }}
                />
              ))}
            </ul>
          )}
        </fieldset>
        {adapter !== null && (
          <div className={styles.actions}>
            <button
              type="button"
              className={current === null ? controls.primary : controls.button}
              aria-disabled={preview.isPending ? 'true' : undefined}
              onClick={() => {
                if (preview.isPending) return;
                compile.reset();
                setCompiled(null);
                preview.mutate(selection, {
                  onSuccess: (body) => {
                    setPreviewed({ key, body });
                  },
                });
              }}
            >
              {preview.isPending ? t('agentBuilder.previewing') : t('agentBuilder.preview')}
            </button>
            {current === null && <span className={styles.meta}>{t('agentBuilder.previewFirst')}</span>}
          </div>
        )}
        {preview.isError && <Denied error={preview.error} />}
      </section>

      {current !== null && (
        <DryRun
          body={current}
          project={project}
          busy={compile.isPending}
          compiled={compiled}
          error={compile.isError ? compile.error : null}
          onCompile={() => {
            if (compile.isPending) return;
            compile.mutate(selection, {
              onSuccess: (body) => {
                setCompiled(body);
              },
            });
          }}
        />
      )}
    </>
  );
}

function ChoiceRow({
  c,
  type,
  checked,
  onChange,
}: {
  c: Choice;
  type: 'radio' | 'checkbox';
  checked: boolean;
  onChange: () => void;
}) {
  const { t } = useTranslation();
  const id = useId();
  return (
    <li className={checked ? `${styles.choice} ${styles.choiceOn}` : styles.choice}>
      <input
        id={id}
        type={type}
        name={type === 'radio' ? 'agent-adapter' : undefined}
        className={styles.box}
        checked={checked}
        onChange={onChange}
        aria-describedby={`${id}-about`}
      />
      <div className={styles.choiceBody}>
        <label htmlFor={id} className={styles.choiceLabel}>
          <span className={styles.slug}>{c.slug}</span> <span className={styles.version}>{c.version}</span>
        </label>
        <div id={`${id}-about`} className={styles.about}>
          <span className={styles.meta}>{kindLabel(t, c.kind)}</span>
          {c.channel !== null && <ChannelBadge channel={c.channel} />}
          {c.reason !== null && <span className={styles.reason}>{reasonText(t, c.reason)}</span>}
        </div>
        <CapabilityMarks caps={c.capabilities} none={null} />
      </div>
    </li>
  );
}

function changeLabel(t: TFunction, change: string): string {
  switch (change) {
    case 'added':
      return t('agentBuilder.change.added');
    case 'modified':
      return t('agentBuilder.change.modified');
    case 'removed':
      return t('agentBuilder.change.removed');
    default:
      return change;
  }
}

function DryRun({
  body,
  project,
  busy,
  compiled,
  error,
  onCompile,
}: {
  body: BuildPreview;
  project: string;
  busy: boolean;
  compiled: BuildCompiled | null;
  /** The failed "Create the merge request", shown at its button. */
  error: unknown;
  onCompile: () => void;
}) {
  const { t } = useTranslation();
  const widening = body.parts.filter((p) => p.capabilities.includes('permissions'));
  // The diff appears below the button: the focus goes to its heading, so a
  // keyboard or screen reader user lands on it (and hears it).
  const heading = useRef<HTMLHeadingElement>(null);
  useEffect(() => {
    heading.current?.focus();
  }, [body]);
  const mr = compiled?.merge_request ?? null;
  return (
    <section className={styles.section} aria-labelledby="agent-dry-run">
      <h2 id="agent-dry-run" className={styles.sectionTitle} ref={heading} tabIndex={-1}>
        {t('agentBuilder.dryRun')}
      </h2>
      <p className={styles.lead}>
        {body.changes.length === 0
          ? t('agentBuilder.noChanges', { branch: body.target_branch })
          : t('agentBuilder.changesOn', { branch: body.target_branch, n: body.changes.length })}
      </p>
      {widening.map((p) => (
        <p key={p.slug} className={styles.danger}>
          {t('agentBuilder.widens', { slug: p.slug })}
        </p>
      ))}
      {body.changes.length > 0 && (
        <ul className={styles.changes}>
          {body.changes.map((c) => (
            <li key={`${c.change}:${c.path}`}>
              <span className={c.change === 'removed' ? styles.removed : c.change === 'added' ? styles.added : styles.modified}>
                {changeLabel(t, c.change)}
              </span>
              <code className={styles.path}>{c.path}</code>
            </li>
          ))}
        </ul>
      )}
      <h3 className={styles.subTitle}>{t('agentBuilder.partsInBuild')}</h3>
      <ul className={styles.built}>
        {body.parts.map((p) => (
          <li key={p.slug}>
            <span className={styles.slug}>{p.slug}</span>{' '}
            <span className={styles.meta}>{t('agentBuilder.kindVersion', { kind: kindLabel(t, p.kind), version: p.version })}</span>{' '}
            <ChannelBadge channel={p.channel} />
            <CapabilityMarks caps={p.capabilities} none={null} />
          </li>
        ))}
      </ul>
      <p className={styles.meta}>
        {t('agentBuilder.lockDigest')} <code className={styles.path}>{body.lock_digest}</code>
      </p>
      {body.changes.length > 0 && compiled === null && (
        <div className={styles.actions}>
          <button type="button" className={controls.primary} aria-disabled={busy ? 'true' : undefined} onClick={onCompile}>
            {busy ? t('agentBuilder.compiling') : t('agentBuilder.compile')}
          </button>
          <span className={styles.meta}>{t('agentBuilder.compileHint')}</span>
        </div>
      )}
      {error !== null && <Denied error={error} />}
      <div role="status" className={styles.result}>
        {compiled !== null &&
          (mr !== null ? (
            <p>
              {t('agentBuilder.compiled', { iid: mr.iid })}{' '}
              <Link to={pullUrl(project, mr.iid)}>{t('agentBuilder.openMr', { iid: mr.iid })}</Link>
            </p>
          ) : (
            <p>
              {t('agentBuilder.committedOnly', { branch: compiled.branch })}{' '}
              {compiled.merge_request_error?.message}
            </p>
          ))}
      </div>
    </section>
  );
}

function stateLabel(t: TFunction, state: string): string {
  switch (state) {
    case 'succeeded':
      return t('agentBuilder.state.succeeded');
    case 'failed':
      return t('agentBuilder.state.failed');
    default:
      return t('agentBuilder.state.requested');
  }
}

function History({ projectId, project }: { projectId: number; project: string }) {
  const { t, i18n } = useTranslation();
  const status = useBuilderStatus(projectId, true);
  return (
    <section className={styles.section} aria-labelledby="agent-history">
      <h2 id="agent-history" className={styles.sectionTitle}>
        {t('agentBuilder.history')}
      </h2>
      {status.isPending && <SkeletonList rows={2} />}
      {status.isError && (
        <QueryError
          error={status.error}
          onRetry={() => {
            void status.refetch();
          }}
        />
      )}
      {status.isSuccess &&
        (status.data.builds.length === 0 ? (
          <p className={styles.hint}>{t('agentBuilder.noHistory')}</p>
        ) : (
          <div className={settings.scroll} role="region" aria-label={t('agentBuilder.history')} tabIndex={0}>
            <table className={settings.table}>
              <caption className={settings.srOnly}>{t('agentBuilder.history')}</caption>
              <thead>
                <tr>
                  <th scope="col">{t('agentBuilder.col.when')}</th>
                  <th scope="col">{t('agentBuilder.col.kind')}</th>
                  <th scope="col">{t('agentBuilder.col.state')}</th>
                  <th scope="col">{t('agentBuilder.col.mr')}</th>
                  <th scope="col">{t('agentBuilder.col.requestId')}</th>
                </tr>
              </thead>
              <tbody>
                {status.data.builds.map((b: BuildRecord) => (
                  <tr key={b.id}>
                    <td>{formatDateTime(b.created_at, i18n.language)}</td>
                    <td>{b.dry_run ? t('agentBuilder.kindDryRun') : t('agentBuilder.kindMr')}</td>
                    <td>{stateLabel(t, b.state)}</td>
                    <td>
                      {b.merge_request_iid === null ? null : (
                        <Link to={pullUrl(project, b.merge_request_iid)}>
                          {t('agentBuilder.mrRef', { iid: b.merge_request_iid })}
                        </Link>
                      )}
                    </td>
                    <td>
                      <code>{b.request_id}</code>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        ))}
    </section>
  );
}
