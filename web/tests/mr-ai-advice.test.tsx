// The AI advice on the merge request screen (plan 19 §3.1 / §4, L5-6;
// ADR-0039 amendments L5-5 and L5-6): the local LLM's first pass as advice
// — labelled 「AI の助言（提案）」 in words (never the colour alone), the
// summary, the findings with their evidence, the deep-review mark (code
// rules first, as the server orders it), the stale reason, the confidential
// "withheld" line and "no advice now". Plain text only. 「役に立った /
// 誤り」 per finding only for a reader the server says may vote. Synthetic
// data.
import axe from 'axe-core';
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { aiAdviceVisible, aiReviewPollInterval, AI_REVIEW_POLL_MS } from '../src/api/aiReview';
import type { Language } from '../src/i18n';
import { apiError, mockServer, type Handler } from './mock';
import { MR_API, MR_SCREENS, SHOWN_ADVICE, aiReview, mrRoutes } from './mr-fixtures';
import { json, renderApp } from './render';

const VIEW = `${MR_SCREENS}/pull/3`;
const AI_API = `${MR_API}/3/ai-review`;
const FEEDBACK_API = `${MR_API}/3/ai-review/feedback`;

async function open(answer: Handler, lang: Language = 'en', over: Record<string, Handler> = {}) {
  const server = mockServer(mrRoutes({ [`GET ${AI_API}`]: answer, ...over }));
  const view = await renderApp({ fetch: server.fetch, path: VIEW, lang });
  await screen.findByRole('heading', { level: 1, name: /Synthetic change 3/ });
  return { server, view };
}

const section = async () => screen.findByTestId('mr-ai-advice');

const shown = (over: Record<string, unknown> = {}, advice: Record<string, unknown> = {}) =>
  aiReview({ mode: 'advisory', can_feedback: true, ...over }, { ...SHOWN_ADVICE, ...advice });

async function violations(root: HTMLElement): Promise<string[]> {
  const result = await axe.run(root, {
    runOnly: { type: 'tag', values: ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa', 'wcag22aa'] },
    rules: { 'color-contrast': { enabled: false }, region: { enabled: false } },
  });
  return result.violations.map((v) => `${v.id}: ${v.help}`);
}

describe('AI advice: when it is shown and polled', () => {
  it('a project with the first pass off and nothing made: no section at all', async () => {
    const { server } = await open(() => json(200, aiReview()));
    await waitFor(() => {
      expect(server.to('GET', AI_API).length).toBeGreaterThan(0);
    });
    await waitFor(() => {
      expect(screen.queryByText(/Loading/)).toBeNull();
    });
    expect(screen.queryByTestId('mr-ai-advice')).toBeNull();
    expect(server.unmatched).toEqual([]);
  });

  it('visible when the mode is on, or anything was made', () => {
    expect(aiAdviceVisible(aiReview())).toBe(false);
    expect(aiAdviceVisible(aiReview({ mode: 'advisory' }))).toBe(true);
    expect(aiAdviceVisible(aiReview({}, { state: 'shown' }))).toBe(true);
    expect(aiAdviceVisible(aiReview({ check: { status: 'completed', conclusion: 'error' } }))).toBe(true);
  });

  it('read again only while the head waits for its first pass', () => {
    expect(aiReviewPollInterval(undefined, true)).toBe(false);
    expect(aiReviewPollInterval(aiReview(), true)).toBe(false);
    const waiting = aiReview({ mode: 'advisory', check: { status: 'queued', conclusion: null } });
    expect(aiReviewPollInterval(waiting, true)).toBe(AI_REVIEW_POLL_MS);
    // Not asked yet (no run on the head): the worker looks every 30 s.
    expect(aiReviewPollInterval(aiReview({ mode: 'required' }), true)).toBe(AI_REVIEW_POLL_MS);
    // A merged or closed merge request is never reviewed again.
    expect(aiReviewPollInterval(waiting, false)).toBe(false);
    const done = aiReview({ mode: 'advisory', check: { status: 'completed', conclusion: 'neutral' } });
    expect(aiReviewPollInterval(done, true)).toBe(false);
  });
});

describe('AI advice: the panel', () => {
  it('none: labelled advice in words, and "no advice now"', async () => {
    await open(() => json(200, aiReview({ mode: 'advisory' })));
    const box = await section();
    expect(within(box).getByRole('heading', { level: 2, name: 'AI review (first pass)' })).toBeTruthy();
    expect(within(box).getByText('AI advice (suggestion)')).toBeTruthy();
    expect(within(box).getByTestId('ai-advice-none').textContent).toBe('There is no AI advice now.');
    expect(within(box).queryByRole('button')).toBeNull();
  });

  it('ja: 「AI の助言（提案）」 and 「AI の助言は今ありません」', async () => {
    await open(() => json(200, aiReview({ mode: 'advisory' })), 'ja');
    const box = await section();
    expect(within(box).getByText('AI の助言（提案）')).toBeTruthy();
    expect(within(box).getByTestId('ai-advice-none').textContent).toContain('AI の助言は今ありません');
  });

  it('shown: the summary, the deep-review mark, the findings with their evidence and the label', async () => {
    await open(() => json(200, shown()));
    const box = await section();
    expect(box.getAttribute('data-state')).toBe('shown');
    expect(within(box).getByText('AI advice (suggestion)')).toBeTruthy();
    expect(within(box).getByTestId('ai-advice-summary').textContent).toBe(
      'Changes the tolerance check.\nTouches the orders table.',
    );
    // The deep-review mark comes before the findings, with the server's
    // reasons as written (code rules first).
    const deep = within(box).getByTestId('ai-advice-deep');
    expect(deep.textContent).toContain('A deeper review (Claude or a person) is recommended');
    expect(deep.textContent).toContain('by code rules: a database migration. The model adds');
    const findings = within(box).getAllByTestId('ai-advice-finding');
    expect(findings).toHaveLength(2);
    const first = findings[0] as HTMLElement;
    expect(first.textContent).toContain('High');
    expect(first.textContent).toContain('src/orders.rs line 41');
    expect(first.textContent).toContain('A missing size panics on a production path.');
    expect(within(first).getByTestId('ai-advice-quote').textContent).toBe('size.unwrap()');
    expect(first.textContent).toContain('Evidence (quoted from the change)');
    expect(first.textContent).toContain('Confidence 90%');
    // No line: the file only.
    expect((findings[1] as HTMLElement).textContent).toContain('src/view.rs');
    expect((findings[1] as HTMLElement).textContent).not.toContain('line null');
    expect(within(box).getByText('Model: qwen3:27b')).toBeTruthy();
    expect(within(box).getByText('Prompt version: v1')).toBeTruthy();
    expect(within(box).getByTestId('ai-advice-only').textContent).toContain('never approves or blocks a merge');
    expect(within(box).queryByTestId('ai-advice-stale')).toBeNull();
    expect(within(box).queryByTestId('ai-advice-degraded')).toBeNull();
  });

  it('plain text: markup and Markdown in the advice are shown as written, never rendered', async () => {
    await open(() =>
      json(
        200,
        shown(
          {},
          {
            summary: '<b>bold</b> **strong** [link](http://example.test)',
            findings: [
              {
                severity: 'critical',
                path: 'a.html',
                line: 1,
                quote: '<script>alert(1)</script>',
                message: '<img src=x onerror=alert(1)> _x_',
                confidence: 0.7,
              },
            ],
          },
        ),
      ),
    );
    const box = await section();
    expect(within(box).getByTestId('ai-advice-summary').textContent).toBe(
      '<b>bold</b> **strong** [link](http://example.test)',
    );
    expect(box.querySelector('b, strong, a[href="http://example.test"], img, script')).toBeNull();
    expect(within(box).getByTestId('ai-advice-quote').textContent).toBe('<script>alert(1)</script>');
    expect(within(box).getByText('Critical')).toBeTruthy();
  });

  it('stale: says so and why (another commit), with both commits', async () => {
    await open(() => json(200, shown({}, { stale: true, head_sha: 'c'.repeat(40) })));
    const box = await section();
    const stale = within(box).getByTestId('ai-advice-stale');
    expect(stale.textContent).toContain('This advice is out of date');
    expect(stale.textContent).toContain('cccccccc');
    expect(stale.textContent).toContain('bbbbbbbb');
  });

  it('degraded: "less trustworthy" in words', async () => {
    await open(() => json(200, shown({}, { degraded: true })));
    const box = await section();
    expect(within(box).getByTestId('ai-advice-degraded').textContent).toContain('Less trustworthy advice');
  });

  it('no findings: said plainly, no buttons', async () => {
    await open(() => json(200, shown({}, { findings: [], deep_review: null })));
    const box = await section();
    expect(within(box).getByText('No findings.')).toBeTruthy();
    expect(within(box).queryByTestId('ai-advice-deep')).toBeNull();
    expect(within(box).queryByRole('button')).toBeNull();
  });

  it('withheld: the confidential line only, never words or buttons', async () => {
    await open(() =>
      json(
        200,
        aiReview(
          { mode: 'advisory', can_feedback: false },
          {
            state: 'withheld',
            note: '機密の入力のため表示しません',
            input_class: 'confidential',
            advice_id: 12,
            model: 'qwen3:27b',
            prompt_version: 1,
            created_at: '2026-11-03T01:00:00Z',
          },
        ),
      ),
    );
    const box = await section();
    expect(within(box).getByTestId('ai-advice-withheld').textContent).toBe('Not shown: the input is confidential.');
    expect(within(box).queryAllByTestId('ai-advice-finding')).toHaveLength(0);
    expect(within(box).queryByRole('button')).toBeNull();
  });

  it('the first pass running, and failed (never shown as a success)', async () => {
    await open(() => json(200, aiReview({ mode: 'advisory', check: { status: 'in_progress', conclusion: null } })));
    const box = await section();
    expect(within(box).getByTestId('ai-advice-running').textContent).toContain('The AI is reviewing');
    expect(within(box).getByTestId('ai-advice-none')).toBeTruthy();
  });

  it('a change after the first answer is said once (the advice arrives while the screen is open)', async () => {
    let n = 0;
    const { view } = await open(() => {
      n += 1;
      return json(
        200,
        n === 1 ? aiReview({ mode: 'advisory', check: { status: 'queued', conclusion: null } }) : shown(),
      );
    });
    await within(await section()).findByTestId('ai-advice-running');
    expect(screen.getByTestId('mr-announce').textContent).toBe('');
    await view.queryClient.invalidateQueries({ queryKey: ['ai-review'] });
    await waitFor(() => {
      expect(screen.getByTestId('mr-announce').textContent).toBe('The AI advice has arrived.');
    });
  });

  it('a failed first pass says it failed and what to do', async () => {
    await open(() => json(200, aiReview({ mode: 'advisory', check: { status: 'completed', conclusion: 'error' } })));
    const box = await section();
    const line = within(box).getByTestId('ai-advice-failed');
    expect(line.textContent).toContain('failed');
    expect(line.textContent).toContain('not a pass');
  });

  it('a failed read shows user_message and request_id with a retry', async () => {
    let n = 0;
    await open(() => {
      n += 1;
      return n === 1 ? apiError(500, 'The AI review could not be read.', 'req-ai-1') : json(200, aiReview({ mode: 'advisory' }));
    });
    const box = await section();
    expect(within(box).getByText('The AI review could not be read.')).toBeTruthy();
    expect(box.textContent).toContain('req-ai-1');
    fireEvent.click(within(box).getByRole('button', { name: /Try again|Retry/ }));
    expect(await within(await section()).findByTestId('ai-advice-none')).toBeTruthy();
  });
});

describe('AI advice: 「役に立った / 誤り」', () => {
  it('no buttons for a reader who may not vote', async () => {
    await open(() => json(200, shown({ can_feedback: false })));
    const box = await section();
    expect(within(box).getAllByTestId('ai-advice-finding')).toHaveLength(2);
    expect(within(box).queryByRole('button', { name: /Useful|Wrong/ })).toBeNull();
  });

  it('one pair per finding; the reader\'s own verdict is pressed; a vote is sent and said', async () => {
    const { server } = await open(
      () => json(200, shown({}, { my_feedback: [{ finding_index: 1, verdict: 'useful' }] })),
      'en',
      {
        [`POST ${FEEDBACK_API}`]: () =>
          json(200, { advice_id: 12, finding_index: 0, verdict: 'wrong', outcome: 'recorded' }),
      },
    );
    const box = await section();
    const groups = within(box).getAllByRole('group', { name: /Was finding \d right\?/ });
    expect(groups).toHaveLength(2);
    const [first, second] = groups as [HTMLElement, HTMLElement];
    expect(within(second).getByRole('button', { name: /^Useful \(finding \d\)$/ }).getAttribute('aria-pressed')).toBe('true');
    expect(within(second).getByRole('button', { name: /^Wrong \(finding \d\)$/ }).getAttribute('aria-pressed')).toBe('false');
    const wrong = within(first).getByRole('button', { name: /^Wrong \(finding \d\)$/ });
    expect(wrong.getAttribute('aria-pressed')).toBe('false');
    fireEvent.click(wrong);
    await waitFor(() => {
      expect(within(first).getByRole('button', { name: /^Wrong \(finding \d\)$/ }).getAttribute('aria-pressed')).toBe('true');
    });
    expect(server.to('POST', FEEDBACK_API).map((c) => c.body)).toEqual([
      { advice_id: 12, finding_index: 0, verdict: 'wrong' },
    ]);
    await waitFor(() => {
      expect(screen.getByTestId('mr-announce').textContent).toContain('Recorded: Wrong');
    });
  });

  it('a refused vote shows user_message and request_id, and nothing is pressed', async () => {
    await open(() => json(200, shown()), 'en', {
      [`POST ${FEEDBACK_API}`]: () => apiError(403, 'You do not have permission to do this.', 'req-vote-9'),
    });
    const box = await section();
    const first = within(box).getAllByRole('group')[0] as HTMLElement;
    fireEvent.click(within(first).getByRole('button', { name: /^Useful \(finding \d\)$/ }));
    expect(await within(box).findByText(/You do not have permission to do this\./)).toBeTruthy();
    expect(box.textContent).toContain('req-vote-9');
    expect(within(first).getByRole('button', { name: /^Useful \(finding \d\)$/ }).getAttribute('aria-pressed')).toBe('false');
  });

  it('ja: 「役に立った」 and 「誤り」', async () => {
    await open(() => json(200, shown()), 'ja');
    const box = await section();
    const first = within(box).getAllByRole('group')[0] as HTMLElement;
    expect(within(first).getByRole('button', { name: '役に立った（指摘 1）' })).toBeTruthy();
    expect(within(first).getByRole('button', { name: '誤り（指摘 1）' })).toBeTruthy();
  });
});

describe('AI advice: axe', () => {
  for (const lang of ['ja', 'en', 'id'] as const) {
    it(`shown, stale and degraded with votes (${lang})`, async () => {
      await open(
        () =>
          json(
            200,
            shown(
              {},
              { stale: true, degraded: true, head_sha: 'c'.repeat(40), my_feedback: [{ finding_index: 0, verdict: 'wrong' }] },
            ),
          ),
        lang,
      );
      const box = await section();
      expect(await violations(box)).toEqual([]);
    });
  }
});
