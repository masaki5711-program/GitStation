// Screens that most visits never open — settings, members, webhooks,
// pipelines, the editor, the act-as popups — are separate chunks, so the
// first load of the app carries only the browsing screens. Each one waits
// behind a skeleton the first time; a link that prefetches its screen
// (features/prefetch.ts) also starts loading its chunk (`preloadScreen`).
import { lazy, Suspense, useState, type ComponentProps, type ComponentType } from 'react';

import { useCheckRuns } from '../api/checks';
import { ErrorBoundary } from '../components/ErrorBoundary';
import { AI_EVAL_CHECK } from '../features/mr/aiEval';
import { SkeletonList } from '../components/Skeleton';

const loaders = {
  settings: () => import('./settings/ProjectSettings'),
  members: () => import('./settings/Members'),
  groupSettings: () => import('./settings/GroupSettings'),
  account: () => import('./settings/Account'),
  webhooks: () => import('./webhooks/WebhooksScreen'),
  deliveries: () => import('./webhooks/DeliveriesScreen'),
  pipelines: () => import('./repo/pipelines/PipelinesScreen'),
  pipeline: () => import('./repo/pipelines/PipelineScreen'),
  mergeQueue: () => import('./repo/mr/MergeQueueScreen'),
  schedules: () => import('./repo/pipelines/SchedulesScreen'),
  editor: () => import('./repo/editor/EditorScreen'),
  actAs: () => import('./confirm/ActAsConfirm'),
  actAsBinding: () => import('./confirm/ActAsBindingConfirm'),
  blame: () => import('./repo/BlameScreen'),
  compare: () => import('./repo/CompareScreen'),
  search: () => import('./search/SearchScreen'),
  activity: () => import('./Activity'),
};

/** A lazy component with a skeleton while its chunk loads. React.lazy
 * keeps a failed load for good, so a failure (offline, a file replaced by a
 * deploy) shows the error with a retry that builds a new lazy component and
 * asks for the chunk again. */
export function lazyScreen<P extends object>(load: () => Promise<ComponentType<P>>) {
  const make = () => lazy(() => load().then((c) => ({ default: c })));
  let Lazy = make();
  return function LazyScreen(props: P) {
    const [attempt, setAttempt] = useState(0);
    return (
      <ErrorBoundary
        key={attempt}
        onRetry={() => {
          Lazy = make();
          setAttempt((a) => a + 1);
        }}
      >
        <Suspense fallback={<SkeletonList />}>
          <Lazy {...(props as ComponentProps<typeof Lazy>)} />
        </Suspense>
      </ErrorBoundary>
    );
  };
}

const screen = lazyScreen;

export const ProjectSettings = screen(() => loaders.settings().then((m) => m.ProjectSettings));
export const MembersScreen = screen(() => loaders.members().then((m) => m.MembersScreen));
export const GroupViews = screen(() => loaders.groupSettings().then((m) => m.GroupViews));
export const Account = screen(() => loaders.account().then((m) => m.Account));
export const WebhooksScreen = screen(() => loaders.webhooks().then((m) => m.WebhooksScreen));
export const DeliveriesScreen = screen(() => loaders.deliveries().then((m) => m.DeliveriesScreen));
export const PipelinesScreen = screen(() => loaders.pipelines().then((m) => m.PipelinesScreen));
export const PipelineScreen = screen(() => loaders.pipeline().then((m) => m.PipelineScreen));
export const MergeQueueScreen = screen(() => loaders.mergeQueue().then((m) => m.MergeQueueScreen));
export const SchedulesScreen = screen(() => loaders.schedules().then((m) => m.SchedulesScreen));
export const EditorScreen = screen(() => loaders.editor().then((m) => m.EditorScreen));
export const ActAsConfirm = screen(() => loaders.actAs().then((m) => m.ActAsConfirm));
export const ActAsBindingConfirm = screen(() => loaders.actAsBinding().then((m) => m.ActAsBindingConfirm));

/** The chunk of a project screen word (`/-/{word}`), if it is a lazy one. */
const BY_WORD: Record<string, (() => Promise<unknown>) | undefined> = {
  settings: loaders.settings,
  members: loaders.members,
  webhooks: loaders.webhooks,
  pipelines: loaders.pipelines,
  'merge-queue': loaders.mergeQueue,
  edit: loaders.editor,
  new: loaders.editor,
};

/** Starts loading the chunk of a screen word ahead of the click. A failed
 * preload is only a missed head start: the screen's own `lazy` import asks
 * again on render, and its failure shows with a retry (`lazyScreen`). */
export function preloadScreen(word: string): void {
  const load = BY_WORD[word];
  if (load !== undefined) void load().catch(() => undefined);
}

/** Every lazy chunk (tests load them up front, as setup.ts does Markdown). */
export function loadAllScreens(): Promise<unknown[]> {
  return Promise.all(Object.values(loaders).map((l) => l()));
}
export const BlameScreen = screen(() => loaders.blame().then((m) => m.BlameScreen));
export const CompareScreen = screen(() => loaders.compare().then((m) => m.CompareScreen));
export const SearchScreen = screen(() => loaders.search().then((m) => m.SearchScreen));
// The full activity list is opened from Home's "see all" now and then;
// split out to keep the first load within its budget (plan 20 K5).
export const Activity = screen(() => loaders.activity().then((m) => m.Activity));

// The MR screen's AI evaluation section (plan 17 P17-9) is its own chunk:
// most merge requests have no `forge/ai-eval` run, so the chunk loads only
// when the head's check runs (the same query as the check list) carry one.
const AiEvalSectionLazy = lazy(() =>
  import('../features/mr/AiEvalReport').then((m) => ({ default: m.AiEvalSection })),
);
export function AiEvalSection(props: ComponentProps<typeof AiEvalSectionLazy>) {
  const runs = useCheckRuns(props.projectId, props.iid, props.head);
  const has = runs.data?.check_runs.some((r) => r.name === AI_EVAL_CHECK) ?? false;
  if (!has) return null;
  return (
    <ErrorBoundary>
      <Suspense fallback={null}>
        <AiEvalSectionLazy {...props} />
      </Suspense>
    </ErrorBoundary>
  );
}
