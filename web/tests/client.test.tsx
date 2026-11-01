import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { I18nextProvider } from 'react-i18next';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { ApiFailure, call, failureOf } from '../src/api/client';
import { ErrorNotice } from '../src/components/ErrorNotice';
import { createI18n } from '../src/i18n';
import { fakeFetch, renderApp } from './render';

afterEach(() => {
  vi.restoreAllMocks();
});

const answer = (response: Response) => () =>
  Promise.resolve({ data: undefined, error: undefined, response });

describe('call()', () => {
  it('rethrows an abort untouched (a cancelled query is not a failure)', async () => {
    const abort = new DOMException('aborted', 'AbortError');
    await expect(call(() => Promise.reject(abort))).rejects.toBe(abort);
  });

  it('a TypeError from fetch is a network failure and keeps the cause', async () => {
    const cause = new TypeError('Failed to fetch');
    const err = await call(() => Promise.reject(cause)).catch((e: unknown) => e);
    expect(err).toBeInstanceOf(ApiFailure);
    expect((err as ApiFailure).failure).toEqual({ kind: 'network' });
    expect((err as ApiFailure).cause).toBe(cause);
  });

  it('any other throw is unexpected, logged once with the cause', async () => {
    const log = vi.spyOn(console, 'error').mockImplementation(() => undefined);
    const cause = new SyntaxError('Unexpected token < in JSON');
    const err = await call(() => Promise.reject(cause)).catch((e: unknown) => e);
    expect((err as ApiFailure).failure).toEqual({
      kind: 'unexpected',
      status: null,
      requestId: null,
    });
    expect((err as ApiFailure).cause).toBe(cause);
    expect(log).toHaveBeenCalledTimes(1);
    expect(log.mock.calls[0]?.[1]).toBe(cause);
  });

  it('a 2xx answer without a body is unexpected and logged', async () => {
    const log = vi.spyOn(console, 'error').mockImplementation(() => undefined);
    const err = await call(answer(new Response(null, { status: 204 }))).catch((e: unknown) => e);
    expect((err as ApiFailure).failure.kind).toBe('unexpected');
    expect(log).toHaveBeenCalledTimes(1);
  });

  it('failureOf maps a foreign error to unexpected', () => {
    expect(failureOf(new Error('bug'))).toEqual({
      kind: 'unexpected',
      status: null,
      requestId: null,
    });
  });
});

describe('home with a body that is not JSON', () => {
  it('shows unexpected, never the raw text, and logs once', async () => {
    const log = vi.spyOn(console, 'error').mockImplementation(() => undefined);
    const { fetch } = fakeFetch(
      () => new Response('not json', { status: 200, headers: { 'content-type': 'application/json' } }),
    );
    const { container } = await renderApp({ fetch });
    await screen.findByRole('alert');
    expect(container.textContent).toContain('思いがけない応答がありました');
    expect(container.textContent).not.toContain('not json');
    expect(log).toHaveBeenCalledTimes(1);
  });
});

describe('ErrorNotice copy', () => {
  async function notice() {
    const i18n = await createI18n('en');
    render(
      <I18nextProvider i18n={i18n}>
        <ErrorNotice failure={{ kind: 'api', status: 503, userMessage: 'Busy', requestId: 'req-1' }} />
      </I18nextProvider>,
    );
  }

  it('says when the clipboard refuses', async () => {
    Object.defineProperty(navigator, 'clipboard', {
      configurable: true,
      value: { writeText: () => Promise.reject(new Error('denied')) },
    });
    await notice();
    fireEvent.click(screen.getByRole('button', { name: 'Copy the request ID' }));
    await waitFor(() => {
      expect(screen.getByRole('button', { name: /Could not copy/ })).toBeTruthy();
    });
  });

  it('says when there is no clipboard (not a secure context)', async () => {
    Object.defineProperty(navigator, 'clipboard', { configurable: true, value: undefined });
    await notice();
    fireEvent.click(screen.getByRole('button', { name: 'Copy the request ID' }));
    expect(screen.getByRole('button', { name: /Could not copy/ })).toBeTruthy();
  });

  it('says when it copied', async () => {
    const writeText = vi.fn(() => Promise.resolve());
    Object.defineProperty(navigator, 'clipboard', { configurable: true, value: { writeText } });
    await notice();
    fireEvent.click(screen.getByRole('button', { name: 'Copy the request ID' }));
    await waitFor(() => {
      expect(screen.getByRole('button', { name: 'Copied' })).toBeTruthy();
    });
    expect(writeText).toHaveBeenCalledWith('req-1');
  });
});
