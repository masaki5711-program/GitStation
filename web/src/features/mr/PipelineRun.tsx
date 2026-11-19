// "Run the pipeline on the current target" (plan 18 S5, ADR-0037). Since a
// target move starts no merge request pipeline of its own, a merge request
// whose required `ci/pipeline` ran on an older target (or not at all) offers
// this button; the server runs the pipeline on the merge result of the
// current target (`POST …/merge-requests/{iid}/pipeline`, PipelineCreate).
// Its answer is one polite line: started, already started, or the merge
// result is prepared first. A refusal shows its user_message and request id.
import type { TFunction } from 'i18next';
import { useTranslation } from 'react-i18next';

import { useRunMergePipeline } from '../../api/mergeRequests';
import type { MergeRequestPipelineRun, MergeStatus } from '../../api/types';
import controls from '../../components/controls.module.css';
import { ActionError } from './ActionError';
import { MERGE_RESULT_CHECK } from './blockedReason';
import styles from './Mr.module.css';

/** Whether the run is worth offering: the target requires `ci/pipeline` and
 * no fresh run of it exists — stale (an older target), or none while the
 * merge is clean (a carried result has no merge result yet; the server
 * prepares it). Not while it waits for a conflict-free merge result. */
export function pipelineRunOffered(status: MergeStatus): boolean {
  if (!status.viewer.can_run_pipeline) return false;
  const check = status.checks.find((c) => c.name === MERGE_RESULT_CHECK);
  if (check === undefined || check.waits_for_merge_result) return false;
  return check.stale || check.status === null;
}

export function runLine(t: TFunction, run: MergeRequestPipelineRun): string {
  switch (run.outcome) {
    case 'queued':
      return t('mr.pipelineRun.queued');
    case 'exists':
      return t('mr.pipelineRun.exists');
    default:
      return t('mr.pipelineRun.computing');
  }
}

export function PipelineRunControl({
  projectId,
  iid,
  status,
  open,
}: {
  projectId: number;
  iid: number;
  status: MergeStatus;
  open: boolean;
}) {
  const { t } = useTranslation();
  const run = useRunMergePipeline(projectId, iid);
  const offered = open && pipelineRunOffered(status);
  if (!offered && !run.isSuccess && !run.isError) return null;
  return (
    <div className={styles.queue} data-testid="mr-pipeline-run">
      {offered && (
        <div className={styles.actions}>
          <button
            type="button"
            className={controls.button}
            aria-disabled={run.isPending ? 'true' : undefined}
            onClick={() => {
              // A second press while the first is answered asks nothing more.
              if (run.isPending) return;
              run.mutate();
            }}
          >
            {run.isPending ? t('mr.pipelineRun.running') : t('mr.pipelineRun.run')}
          </button>
        </div>
      )}
      <p role="status" className={styles.nextHint} data-testid="mr-pipeline-run-status">
        {run.isSuccess ? runLine(t, run.data) : ''}
      </p>
      {run.isError && (
        <ActionError
          projectId={projectId}
          iid={iid}
          error={run.error}
          onReload={() => {
            run.reset();
          }}
        />
      )}
    </div>
  );
}
