// Synthetic data for the webhook and notification screens (U3a): the shapes
// of docs/api/webhooks.md and the N1 preferences API. Synthetic only.
import { PROJECT, RESOLVE_PROJECT, signedIn, type Handler } from './mock';
import { json } from './render';

export const API = '/api/v1/projects/7';
export const HOOKS = `${API}/webhooks`;

export function webhook(id: number, over: Record<string, unknown> = {}) {
  return {
    id,
    project_id: 7,
    origin: `https://ci${id}.example.test:8443`,
    enabled: true,
    event_types: ['merge_request.*'],
    disabled_reason: null,
    consecutive_failures: 0,
    failing_since: null,
    last_delivery_at: '2026-11-03T01:15:00Z',
    owner_id: 5,
    previous_secret_valid_until: null,
    secret_version: 1,
    created_at: '2026-11-01T00:00:00Z',
    updated_at: '2026-11-01T00:00:00Z',
    ...over,
  };
}

export const SECRET = 'whsec_SYNTHETICSECRETFORTESTS0123456789';

export function delivery(id: number, over: Record<string, unknown> = {}) {
  return {
    id,
    delivery_id: `0b9c1f0e-0000-4000-8000-${String(id).padStart(12, '0')}`,
    event_seq: id * 10,
    event_id: `evt_${String(id).padStart(32, '0')}`,
    event_type: 'merge_request.merged',
    state: 'succeeded',
    attempts: 1,
    redeliveries: 0,
    created_at: '2026-11-03T01:15:00Z',
    last_attempt_at: '2026-11-03T01:15:01Z',
    finished_at: '2026-11-03T01:15:01Z',
    last_status: 200,
    last_reason: null,
    last_error_code: null,
    last_duration_ms: 120,
    ...over,
  };
}

/** The route table of the webhook screens as a Maintainer. */
export function hookRoutes(over: Record<string, Handler> = {}): Record<string, Handler> {
  return {
    ...signedIn(),
    'GET /api/v1/namespaces/resolve': () => json(200, RESOLVE_PROJECT),
    [`GET ${API}`]: () => json(200, { ...PROJECT, role: 'maintainer' }),
    [`GET ${HOOKS}`]: () => json(200, { items: [webhook(3)] }),
    ...over,
  };
}

export const PREFS = {
  email_enabled: true,
  language: null,
  kinds: ['review.requested', 'merge_request.merged', 'mention'],
  muted_kinds: ['mention'],
};

export const PREFS_URL = '/api/v1/user/notification-preferences';
