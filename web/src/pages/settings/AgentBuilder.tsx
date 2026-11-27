// Project settings → "Agent" (/{project}/-/settings/agent; plan 16 A8, §6):
// build the project's Claude Code configuration (.claude/, CLAUDE.md,
// .mcp.json) from signed marketplace parts.
//
// - The update band (plan 16 A12, a Developer's): `GET …/agent-builder/
//   updates` — the lock's parts at the default branch's tip whose version
//   is revoked (named in words, first) or has a newer stable version, the
//   parts the update adds or drops, and "Create the update merge request"
//   (`POST …/update`) only when the API says `can_update`.
// - The local LLM's reasons (plan 16 A11): each part's `llm_reason` and the
//   suggested parts (never checked; with their capability marks), labelled
//   「AI の助言（提案）」 in words, plain text only; a note says when there
//   are no reasons now and why, and the recommendation is read again by
//   itself while they are `pending`.
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
  mayBuild,
  mayRecommend,
  updateChanges,
  useAskUpdate,
  useBuilderStatus,
  useCompile,
  useParts,
  usePreview,
  useRecommendation,
  useUpdates,
  type BuildCompiled,
  type BuildPreview,
  type BuildRecord,
  type PartSummary,
  type Recommendation,
  type RecommendLlm,
  type SelectionItem,
  type UpdatePart,
  type UpdateQueued,
} from '../../api/marketplace';
import { projectQuery } from '../../api/namespaces';
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
  /** The local LLM's reason (plain text, an AI proposal); `null`: none. */
  llmReason: string | null;
  /** Suggested by the LLM only (never checked by itself). */
  suggested: boolean;
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

/** The choices: the recommendation first (its order), then the LLM's
 * suggestions (unchecked), then every other catalog part with a live
 * version, by slug. */
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
      llmReason: r.llm_reason ?? null,
      suggested: false,
    });
    seen.add(r.slug);
  }
  for (const s of rec?.suggested ?? []) {
    if (seen.has(s.slug)) continue;
    const c = bySlug.get(s.slug)?.latest;
    out.push({
      slug: s.slug,
      kind: s.kind,
      version: s.version,
      channel: c !== null && c !== undefined && c.version === s.version ? c.channel : null,
      // The API's own marks: the reason is never the only thing read.
      capabilities: s.capabilities,
      reason: null,
      llmReason: s.reason,
      suggested: true,
    });
    seen.add(s.slug);
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
      llmReason: null,
      suggested: false,
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

/** The selection's identity, whatever its order (the LLM's reasons may
 * reorder the parts: the Dry-Run of the same choice stays valid). */
function selectionKey(selection: readonly SelectionItem[]): string {
  return JSON.stringify([...selection].sort((a, b) => a.slug.localeCompare(b.slug)));
}

/** The recommendation's identity: its parts and versions (not the LLM's
 * reasons or order, which may arrive later — the choice is kept). */
function recommendationKey(rec: Recommendation): string {
  return rec.parts
    .map((p) => `${p.slug}@${p.version}`)
    .sort()
    .join(',');
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

  return (
    <>
      {canBuild && <UpdateBand projectId={projectId} project={project} />}
      {rec.isSuccess && (
        <AiNote
          llm={rec.data.llm}
          source={rec.data.source}
          degraded={rec.data.degraded}
          busy={rec.isFetching}
          onAgain={() => {
            void rec.refetch();
          }}
        />
      )}
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
            // A new recommendation starts the choice again (its defaults);
            // reasons arriving later keep it.
            key={recommendationKey(rec.data)}
            projectId={projectId}
            project={project}
            choices={choicesOf(rec.data, catalog.data.parts)}
          />
        ) : (
          <>
            <Recommended choices={choicesOf(rec.data, []).filter((c) => c.reason !== null || c.suggested)} />
            <p className={styles.hint}>{t('agentBuilder.developerOnly')}</p>
          </>
        )
      )}
      {canBuild && <History projectId={projectId} project={project} />}
    </>
  );
}

/** One changed part of the update in words (literal keys). */
function updateLine(t: TFunction, p: UpdatePart): string {
  const from = p.locked_version ?? '';
  const to = p.next_version ?? '';
  if (p.locked_revoked) {
    return p.next_version === null
      ? t('agentBuilder.update.itemRevokedDropped', { slug: p.slug, from })
      : t('agentBuilder.update.itemRevoked', { slug: p.slug, from, to });
  }
  if (p.locked_version === null) return t('agentBuilder.update.itemAdded', { slug: p.slug, to });
  if (p.next_version === null) return t('agentBuilder.update.itemDropped', { slug: p.slug, from });
  return t('agentBuilder.update.item', { slug: p.slug, from, to });
}

/** The update band (a Developer's read): what the update changes and, when
 * the API says the caller may, "Create the update merge request". */
function UpdateBand({ projectId, project }: { projectId: number; project: string }) {
  const { t } = useTranslation();
  const id = useId();
  const updates = useUpdates(projectId, true);
  const ask = useAskUpdate(projectId);
  const [asked, setAsked] = useState<UpdateQueued | null>(null);
  // The button goes away once asked: the focus moves to the answer.
  const result = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (asked !== null) result.current?.focus();
  }, [asked]);
  if (updates.isPending) {
    return (
      <p className={styles.hint} role="status">
        {t('agentBuilder.update.checking')}
      </p>
    );
  }
  if (updates.isError) {
    return (
      <QueryError
        error={updates.error}
        onRetry={() => {
          void updates.refetch();
        }}
      />
    );
  }
  const body = updates.data;
  if (!body.available) {
    return (
      <p className={styles.hint}>
        {body.reason === 'no_lock' ? t('agentBuilder.update.noLock') : t('agentBuilder.update.upToDate')}
      </p>
    );
  }
  const changes = updateChanges(body.parts);
  const revoked = changes.some((p) => p.locked_revoked);
  return (
    <section className={revoked ? `${styles.band} ${styles.bandRevoked}` : styles.band} aria-labelledby={id}>
      <h2 id={id} className={styles.bandTitle}>
        {revoked ? t('agentBuilder.update.titleRevoked') : t('agentBuilder.update.title')}
      </h2>
      <ul className={styles.bandList}>
        {changes.map((p) => (
          <li key={p.slug}>
            {p.locked_revoked && (
              <>
                <span className={styles.revokedMark}>{t('agentBuilder.update.revokedMark')}</span>{' '}
              </>
            )}
            {updateLine(t, p)}
          </li>
        ))}
      </ul>
      {body.preview !== null && (
        <p className={styles.bandMeta}>{t('agentBuilder.update.files', { n: body.preview.changes.length })}</p>
      )}
      {body.can_update ? (
        asked === null && (
          <>
            <p className={styles.bandNext}>{t('agentBuilder.update.next')}</p>
            <div className={styles.actions}>
              <button
                type="button"
                className={controls.primary}
                aria-disabled={ask.isPending ? 'true' : undefined}
                onClick={() => {
                  if (ask.isPending) return;
                  ask.mutate(undefined, {
                    onSuccess: (queued) => {
                      setAsked(queued);
                    },
                  });
                }}
              >
                {ask.isPending ? t('agentBuilder.update.asking') : t('agentBuilder.update.ask')}
              </button>
            </div>
          </>
        )
      ) : (
        <p className={styles.bandNext}>{t('agentBuilder.update.notAllowed')}</p>
      )}
      {ask.isError && <Denied error={ask.error} />}
      <div role="status" className={styles.result} ref={result} tabIndex={-1}>
        {asked !== null && <AskedLine asked={asked} project={project} />}
      </div>
    </section>
  );
}

function AskedLine({ asked, project }: { asked: UpdateQueued; project: string }) {
  const { t } = useTranslation();
  if (asked.queued) return <p>{t('agentBuilder.update.queued')}</p>;
  if (asked.reason === 'already_open' && asked.merge_request_iid !== null) {
    return (
      <p>
        {t('agentBuilder.update.alreadyOpen', { iid: asked.merge_request_iid })}{' '}
        <Link to={pullUrl(project, asked.merge_request_iid)}>
          {t('agentBuilder.openMr', { iid: asked.merge_request_iid })}
        </Link>
      </p>
    );
  }
  return <p>{t('agentBuilder.update.alreadyAsked')}</p>;
}

/** Why there are no AI reasons now, in words (literal keys). */
function noReasonWhy(t: TFunction, state: string): string {
  switch (state) {
    case 'pending':
      return t('agentBuilder.ai.why.pending');
    case 'unavailable':
      return t('agentBuilder.ai.why.unavailable');
    case 'paused':
      return t('agentBuilder.ai.why.paused');
    case 'refused':
      return t('agentBuilder.ai.why.refused');
    case 'withheld':
      return t('agentBuilder.ai.why.withheld');
    default:
      return t('agentBuilder.ai.why.other');
  }
}

/** The local LLM's part of the recommendation: its summary (plain text) or
 * why there are no reasons now. A live region: reasons arriving are said. */
function AiNote({
  llm,
  source,
  degraded,
  busy,
  onAgain,
}: {
  llm: RecommendLlm;
  source: string;
  degraded: boolean;
  busy: boolean;
  onAgain: () => void;
}) {
  const { t } = useTranslation();
  const id = useId();
  const ready = llm.state === 'ready';
  // Rules only and nothing to say about the LLM: no note.
  if (ready && source !== 'rules+llm' && !degraded) return null;
  const summary = llm.summary ?? null;
  const again = llm.state === 'pending' || llm.state === 'unavailable';
  // A named region; only the sentences are the live part (the button is
  // outside it, so a new answer is said without the button's name).
  return (
    <section aria-labelledby={id} className={styles.ai}>
      <h2 id={id} className={styles.aiTitle}>
        <span className={styles.aiTag}>{t('agentBuilder.ai.label')}</span>
      </h2>
      <div role="status">
        {ready ? (
          <>
            {summary !== null && summary !== '' && <p className={styles.aiText}>{summary}</p>}
            {degraded && <p className={styles.aiText}>{t('agentBuilder.ai.partial')}</p>}
            <p className={styles.meta}>{t('agentBuilder.ai.disclaimer')}</p>
          </>
        ) : (
          <p className={styles.aiText}>{t('agentBuilder.ai.none', { why: noReasonWhy(t, llm.state) })}</p>
        )}
        {busy && <p className={styles.meta}>{t('agentBuilder.ai.reading')}</p>}
      </div>
      {!ready && again && (
        <button
          type="button"
          className={controls.button}
          aria-disabled={busy ? 'true' : undefined}
          onClick={() => {
            if (!busy) onAgain();
          }}
        >
          {t('agentBuilder.ai.again')}
        </button>
      )}
    </section>
  );
}

/** A part's AI reason (plain text), labelled in words. */
function AiReason({ c, id }: { c: Choice; id?: string }) {
  const { t } = useTranslation();
  if (c.llmReason === null) return null;
  return (
    <p id={id} className={styles.aiReason}>
      <span className={styles.aiTag}>{c.suggested ? t('agentBuilder.ai.suggested') : t('agentBuilder.ai.label')}</span>{' '}
      <span>{c.llmReason}</span>
    </p>
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
            <AiReason c={c} />
            {c.suggested && <CapabilityMarks caps={c.capabilities} none={null} />}
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
  const key = selectionKey(selection);
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
        aria-describedby={c.llmReason === null ? `${id}-about` : `${id}-about ${id}-ai`}
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
        <AiReason c={c} id={`${id}-ai`} />
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
