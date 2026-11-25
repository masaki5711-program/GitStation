// The lazy screens: each dynamic import here is a separate chunk that most
// visits never open. This file is also the i18n split boundary
// (scripts/i18n-split.mjs): ja strings used only by these screens (and what
// only they import) leave the main chunk and load with the screen
// (lazyScreen in lazyScreens.tsx waits for them). Only lazy screen imports
// belong here; any other dynamic import counts as part of the first load.
export const loaders = {
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
  adminRedirect: () => import('./AdminRedirect'),
  blame: () => import('./repo/BlameScreen'),
  compare: () => import('./repo/CompareScreen'),
  search: () => import('./search/SearchScreen'),
  activity: () => import('./Activity'),
  wiki: () => import('./wiki/WikiScreen'),
};
