// Synthetic backend for the act-as confirmation popup (U3, ADR-0025 5.4):
// what the screen shows and the person's press (CSRF required). A state may be
// issued once; a second press with it is a 400, as the real server answers.
// Shapes follow src/api/schema.gen.ts (ConfirmationBody, IssuedBody).
export const ACT_AS_HEAD = 'c'.repeat(40);
export const ACT_AS_STATE = 'journeyStateJourneyState0123456789';

export function actAsBackend(csrf, user) {
  const used = new Set();
  let issued = 0;
  function answer(req, url, body) {
    const confirm = /^\/api\/v1\/projects\/7\/merge-requests\/(\d+)\/act-as-confirmation$/.exec(url.pathname);
    if (req.method === 'GET' && confirm) {
      return {
        status: 200,
        body: {
          project_id: 7,
          iid: Number(confirm[1]),
          title: 'Raise the line speed',
          source_branch: 'feat/speed',
          target_branch: 'main',
          head_sha: ACT_AS_HEAD,
          target_sha: 'd'.repeat(40),
          sha_matches: url.searchParams.get('sha') === ACT_AS_HEAD,
          intent: url.searchParams.get('intent') ?? 'approve',
          integration_id: 4,
          integration_kind: 'kanflow',
          integration_name: 'kanflow-svc',
          person_display_name: user.display_name,
          person_username: user.username,
        },
      };
    }
    if (req.method === 'POST' && /^\/-\/confirm\/projects\/7\/merge-requests\/\d+\/assertions$/.test(url.pathname)) {
      if (req.headers['x-gitstation-csrf'] !== csrf) {
        return { status: 403, error: ['The request was rejected.', 'req-act-as-csrf', 'FG-AUTH-0016'] };
      }
      const input = JSON.parse(body);
      if (used.has(input.state)) {
        return { status: 400, error: ['That link was already used.', 'req-act-as-used', 'FG-WEB-0006'] };
      }
      used.add(input.state);
      issued += 1;
      return { status: 201, body: { status: 'issued' } };
    }
    return null;
  }
  return { answer, issued: () => issued };
}
