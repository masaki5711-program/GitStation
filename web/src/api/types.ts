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
