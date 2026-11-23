// Plain words for approvers (ADR-0038 §3): a selector as one line, the
// reason no one can approve, and the next step. Pure (only `t`).
import type { TFunction } from 'i18next';

import type { ApproverSelector } from '../api/approvers';

/** Where a selector looks, in words. */
function whereText(t: TFunction, s: ApproverSelector): string {
  const unit = s.unit ?? '';
  switch (s.relation) {
    case 'requester_section':
      return t('approvers.sel.section');
    case 'requester_department':
      return s.include_descendants === true ? t('approvers.sel.departmentWide') : t('approvers.sel.department');
    case 'unit':
      return s.include_descendants === true
        ? t('approvers.sel.unitWide', { unit })
        : t('approvers.sel.unit', { unit });
  }
}

/** Who in it qualifies, in words. */
function whoText(t: TFunction, s: ApproverSelector): string {
  if (typeof s.min_rank === 'number') return t('approvers.sel.minRank', { rank: s.min_rank });
  if (Array.isArray(s.positions) && s.positions.length > 0) {
    return t('approvers.sel.positions', { list: s.positions.join(', ') });
  }
  return t('approvers.sel.anyone');
}

/** 「申請者の課の 職位ランク 55 以上（いなければ 1 つ上の組織）」. */
export function selectorText(t: TFunction, s: ApproverSelector): string {
  const line = t('approvers.sel.line', { where: whereText(t, s), who: whoText(t, s) });
  return s.escalate === 'parent' ? `${line}${t('approvers.sel.escalate')}` : line;
}

/** Why a rule's selector names no unit (`GET …/approvals` `unresolved`), with
 * the next step, in words. */
export function unresolvedText(t: TFunction, reason: string, detail: string | null): string {
  switch (reason) {
    case 'requester_org_unknown':
      return detail === 'master_stale' || detail === 'master_missing'
        ? t('approvers.none.masterStale')
        : t('approvers.none.authorUnknown');
    case 'requester_no_section':
      return t('approvers.none.noSection');
    case 'requester_no_department':
      return t('approvers.none.noDepartment');
    case 'unit_not_in_master':
      return t('approvers.none.unitMissing');
    default:
      return t('approvers.none.unknown');
  }
}
