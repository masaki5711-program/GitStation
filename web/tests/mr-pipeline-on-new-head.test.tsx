// Plan 18 S5: a target move starts no merge request pipeline of its own.
// On a target that requires `ci/pipeline` the page merges through the queue
// ("Merge when ready", the primary button, its hint as its description) and
// never offers the direct "Merge…"; a stale `ci/pipeline` says which target it
// is as of, the status line names what to press, and "Run the pipeline on the
// current target" asks the server once and says what it did. A direct merge
// refused with FG-CI-0013 `hint=merge_queue` words that hint.
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { ApiFailure, toFailure } from '../src/api/client';
import { hintsQueue } from '../src/features/mr/ActionError';
import { pipelineRunOffered } from '../src/features/mr/PipelineRun';
import { mockServer } from './mock';
import { MR_API, MR_SCREENS, mergeSettings, mrRoutes, status } from './mr-fixtures';
import { json, renderApp } from './render';

const VIEW = `${MR_SCREENS}/pull/3`;
const OLD_TIP = '1'.repeat(40);
const SETTINGS_API = '/api/v1/projects/7/merge-settings';

const viewer = { ...status([]).viewer, can_merge: true, can_run_pipeline: true };

function pipelineCheck(over: Record<string, unknown> = {}) {
  return {
    name: 'ci/pipeline',
    status: 'completed',
    conclusion: 'success',
    stale: false,
    waits_for_merge_result: false,
    target_sha: '2'.repeat(40),
    ...over,
  };
}

/** Green on the current target: mergeable. */
const green = () => ({ ...status([]), checks: [pipelineCheck()], viewer });

/** The run is of the merge result of an older target. */
const stale = () => ({
  ...status([{ reason: 'required_check_stale', detail: 'name=ci/pipeline' }]),
  checks: [pipelineCheck({ stale: true, target_sha: OLD_TIP })],
  viewer,
});

async function open(body: unknown, routes: Parameters<typeof mrRoutes>[0] = {}) {
  const server = mockServer(
    mrRoutes({
      [`GET ${MR_API}/3/status`]: () => json(200, body),
      [`GET ${SETTINGS_API}`]: () => json(200, mergeSettings()),
      ...routes,
    }),
  );
  const view = await renderApp({ fetch: server.fetch, path: VIEW, lang: 'en' });
  await screen.findByRole('heading', { level: 1, name: /Synthetic change 3/ });
  return { server, ...view };
}

describe('a target that requires the merge request pipeline', () => {
  it('offers Merge when ready as the primary button, never the direct Merge', async () => {
    await open(green());
    const button = await screen.findByRole('button', { name: 'Merge when ready' });
    expect(button.className).toContain('primary');
    expect(button.getAttribute('aria-describedby')).not.toBeNull();
    const hint = document.getElementById(button.getAttribute('aria-describedby') ?? '');
    expect(hint?.textContent).toContain('The merge queue tests the change');
    expect(screen.queryByRole('button', { name: 'Merge…' })).toBeNull();
    expect(screen.queryByRole('button', { name: 'Add to merge queue' })).toBeNull();
    // Green on the current target: nothing to run.
    expect(screen.queryByRole('button', { name: 'Run the pipeline on the current target' })).toBeNull();
  });

  it('keeps the direct Merge and the plain queue button elsewhere', async () => {
    await open({ ...status([]), viewer });
    await screen.findByRole('button', { name: 'Merge…' });
    expect(screen.queryByRole('button', { name: 'Merge when ready' })).toBeNull();
  });

  it('says a stale pipeline is as of the older target and what to press next', async () => {
    const { server } = await open(stale(), {
      [`POST ${MR_API}/3/pipeline`]: () =>
        json(202, {
          iid: 3,
          outcome: 'queued',
          merge_result_sha: 'c'.repeat(40),
          target_sha: '2'.repeat(40),
          check_seq: null,
        }),
    });
    await screen.findByText(/as of 11111111 \(an older target\)/);
    await waitFor(() => {
      expect(screen.getByTestId('mr-status-area').textContent).toContain(
        'Press Merge when ready, or run the pipeline on the current target.',
      );
    });
    expect(await screen.findByRole('button', { name: 'Merge when ready' })).toBeTruthy();
    const run = screen.getByRole('button', { name: 'Run the pipeline on the current target' });
    fireEvent.click(run);
    const area = screen.getByTestId('mr-pipeline-run');
    await waitFor(() => {
      expect(within(area).getByRole('status').textContent).toBe(
        'The pipeline was started on the current target.',
      );
    });
    expect(server.to('POST', `${MR_API}/3/pipeline`)).toHaveLength(1);
  });

  it('offers the run only to who may run pipelines, and not while a conflict holds it', () => {
    expect(pipelineRunOffered(stale())).toBe(true);
    expect(
      pipelineRunOffered({ ...stale(), viewer: { ...viewer, can_run_pipeline: false } }),
    ).toBe(false);
    expect(pipelineRunOffered(green())).toBe(false);
    expect(
      pipelineRunOffered({
        ...stale(),
        checks: [pipelineCheck({ status: null, conclusion: null, stale: false, waits_for_merge_result: true, target_sha: null })],
      }),
    ).toBe(false);
  });
});

describe('the queue hint of a refused merge', () => {
  it('is read from the error body and worded, never shown raw', () => {
    const headers = new Headers();
    const refused = toFailure(new Response(null, { status: 409, headers }), {
      error_code: 'FG-CI-0013',
      user_message: 'A required check belongs to the merge result of an older target.',
      request_id: 'req-1',
      hint: 'merge_queue',
    });
    expect(refused).toMatchObject({ kind: 'api', status: 409, hint: 'merge_queue' });
    expect(hintsQueue(new ApiFailure(refused))).toBe(true);
    const other = toFailure(new Response(null, { status: 409 }), {
      error_code: 'FG-REVIEW-0007',
      user_message: 'Blocked.',
      request_id: 'req-2',
      hint: 'something_else',
    });
    expect(other).not.toHaveProperty('hint');
    expect(hintsQueue(new ApiFailure(other))).toBe(false);
  });
});
