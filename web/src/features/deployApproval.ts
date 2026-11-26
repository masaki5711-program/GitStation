// The deployment approval screen's decisions that are not drawing
// (plan 19 §3.4, L5-6). Nothing here reads the AI summary to decide
// anything about the approval: the button depends on the deployment, the
// caller and the environment's approvers only (ADR-0039 amendment L5-8,
// decision 6). The server decides again on every approval.
import type { TFunction } from 'i18next';

import type { ApprovalFacts, ApprovalSummary, Deployment, SummaryAdvice } from '../api/deployments';
import type { DeployApprovers } from '../api/types';

/** Who the caller is to this deployment's approval, as far as the screen
 * can tell. `maybe`: the environment names groups or organisation
 * selectors the screen cannot evaluate; the button is offered and the
 * server decides (FG-DEPLOY-0014 otherwise). */
export type ApproverStanding = 'not_waiting' | 'requester' | 'approver' | 'maybe' | 'not_approver';

const RANK: Record<string, number> = { guest: 0, reporter: 1, developer: 2, maintainer: 3, owner: 4 };
const REPORTER = 1;
const MAINTAINER = 3;
const rank = (role: string | null | undefined) => (role == null ? -1 : (RANK[role] ?? -1));

export function approverStanding(
  d: Deployment,
  me: number,
  role: string | null | undefined,
  approvers: DeployApprovers | undefined,
): ApproverStanding {
  if (d.status !== 'pending_approval') return 'not_waiting';
  // FG-DEPLOY-0009: the requester never approves their own deployment
  // (nor anyone behind the sha: the server knows those, the screen not).
  if (d.requested_by === me) return 'requester';
  // Unknown approvers (still loading or failed): the old rule, a Maintainer.
  if (approvers === undefined || approvers.unrestricted) {
    return rank(role) >= MAINTAINER ? 'approver' : 'not_approver';
  }
  if (approvers.users.some((u) => u.id === me) && rank(role) >= REPORTER) return 'approver';
  const others = approvers.groups.length > 0 || approvers.selectors.length > 0;
  return others && rank(role) >= MAINTAINER ? 'maybe' : 'not_approver';
}

export const offersApprove = (s: ApproverStanding) => s === 'approver' || s === 'maybe';

/** The environment's approvers row for a deployment (`undefined`: none read). */
export function approversOf(
  list: DeployApprovers[] | undefined,
  environment: string,
): DeployApprovers | undefined {
  return list?.find((e) => e.tier === environment);
}

export function environmentLabel(t: TFunction, env: string): string {
  if (env === 'prod') return t('deploy.env.prod');
  if (env === 'stg') return t('deploy.env.stg');
  if (env === 'dev') return t('deploy.env.dev');
  return env;
}

export function deployStatusLabel(t: TFunction, status: string): string {
  switch (status) {
    case 'pending_approval':
      return t('deploy.status.pending_approval');
    case 'queued':
      return t('deploy.status.queued');
    case 'running':
      return t('deploy.status.running');
    case 'succeeded':
      return t('deploy.status.succeeded');
    case 'failed':
      return t('deploy.status.failed');
    case 'rolled_back':
      return t('deploy.status.rolled_back');
    case 'rollback_failed':
      return t('deploy.status.rollback_failed');
    case 'blocked':
      return t('deploy.status.blocked');
    default:
      return status;
  }
}

export type DeployTone = 'success' | 'failed' | 'running' | 'pending' | 'neutral';

export function deployTone(status: string): DeployTone {
  switch (status) {
    case 'succeeded':
      return 'success';
    case 'failed':
    case 'rollback_failed':
    case 'blocked':
      return 'failed';
    case 'running':
      return 'running';
    case 'rolled_back':
      return 'neutral';
    default:
      return 'pending';
  }
}

/** A list of changes is "unknown", never "none", when git could not read
 * the range (`range = unavailable`). */
export type ListState = 'listed' | 'none' | 'unknown';

export function listState(facts: ApprovalFacts, items: readonly unknown[]): ListState {
  if (items.length > 0) return 'listed';
  return facts.range === 'unavailable' ? 'unknown' : 'none';
}

export function staleReasonText(t: TFunction, reason: string | null | undefined): string {
  switch (reason) {
    case 'sha_changed':
      return t('deploy.advice.stale.sha_changed');
    case 'newer_commit':
      return t('deploy.advice.stale.newer_commit');
    case 'facts_changed':
      return t('deploy.advice.stale.facts_changed');
    default:
      return t('deploy.advice.stale.facts_unknown');
  }
}

/** The advice's state as the screen draws it. An unknown state is drawn as
 * "no summary": the facts and the button never wait for it. */
export function adviceState(s: SummaryAdvice): 'shown' | 'withheld' | 'none' {
  if (s.state === 'shown' && typeof s.text === 'string' && s.text !== '') return 'shown';
  if (s.state === 'withheld') return 'withheld';
  return 'none';
}

/** What the approver decides on: the deployment's state, commit and
 * environment, and the facts. The summary is not part of it (a summary
 * that came meanwhile is no reason to ask again). */
export function decisionKey(s: ApprovalSummary): string {
  return JSON.stringify([s.deployment.status, s.deployment.sha, s.deployment.environment, s.facts]);
}
