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
export type Kind = S['KindName'];
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
