// Names for the generated schema types the pages use (schema.gen.ts is
// never edited by hand; `npm run gen:api`).
import type { components } from './schema.gen';

type S = components['schemas'];

export type Project = S['ProjectBody'];
export type ListedGroup = S['ListedGroup'];
export type Group = S['GroupBody'];
export type Session = S['SessionBody'];
export type Resolution = S['Resolution'];
export type RefsBody = S['RefsBody'];
export type CreateProjectBody = S['CreateProject'];
export type CreateGroupBody = S['CreateGroup'];
export type Visibility = S['VisibilityName'];
export type DiffBody = S['DiffBody'];
export type DiffFile = S['DiffFileBody'];
export type Rev = S['RevBody'];
export type RefEntry = S['RefBody'];
export type TreeBody = S['TreeBody'];
export type TreeEntry = S['EntryBody'];
export type BlobBody = S['BlobBody'];
export type CommitSummary = S['CommitBody'];
export type CommitsBody = S['CommitsBody'];
export type CommitDetail = S['CommitDetailBody'];
export type CompareBody = S['CompareBody'];
export type BlameBody = S['BlameBody'];

// Merge requests (U1). `MergeStatus` is the stable status contract
// (`MergeRequestStatusBody`, docs/api/merge-status.md); the schema has a
// different `MergeStatusBody` (the latest merge attempt), not aliased here.
export type MergeRequest = S['MergeRequestBody'];
export type MergeRequestPage = S['MergeRequestPage'];
export type MergeStatus = S['MergeRequestStatusBody'];
export type StatusReason = S['StatusReasonBody'];
export type Discussion = S['DiscussionBody'];
export type DiscussionList = S['DiscussionsBody'];
export type Note = S['NoteBody'];
export type Position = S['PositionBody'];
export type Version = S['VersionBody'];
export type VersionList = S['VersionsBody'];
export type VersionDiff = S['VersionDiffBody'];
export type CreateMergeRequestBody = S['CreateMergeRequest'];

// U2: approve / merge / settings.
export type StatusViewer = S['StatusViewerBody'];
export type StatusApproval = S['StatusApprovalBody'];
export type ApprovalAnswer = S['ApprovalAnswerBody'];
/** One merge attempt (`queued` → … → `applied` / `superseded` / `failed`). */
export type MergeAttempt = S['MergeAttemptBody'];
/** `GET …/merge`: the latest attempt (`merge: null` when none). */
export type LatestMerge = S['MergeStatusBody'];
export type MergeSettings = S['MergeSettingsBody'];
export type PutMergeSettingsBody = S['PutMergeSettings'];
// Plan 13 Q13: the document lint migration setting.
export type DocLintMigration = S['DocLintMigrationBody'];
export type ApprovalRule = S['ApprovalRuleBody'];
export type PutApprovalRuleBody = S['PutApprovalRule'];
export type ProtectedRef = S['ProtectedRefBody'];
export type MergeRule = S['MergeRuleBody'];
export type PutMergeRuleBody = S['PutMergeRule'];

// Documents (plan 13 stage 3).
export type DocumentListBody = S['DocumentListBody'];
export type DocumentSummary = S['DocumentSummaryBody'];
export type DocumentBody = S['DocumentBody'];
export type DocumentSection = S['DocumentSectionBody'];
export type DocumentPiece = S['DocumentPieceBody'];
export type DocumentBlock = S['DocumentBlockBody'];
export type DocumentRef = S['DocumentRefBody'];
