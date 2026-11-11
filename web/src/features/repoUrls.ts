// The addresses of the repository screens and of the server's raw / archive
// routes (ADR-0015): `/{project path}/-/{word}/{ref}[/{path}]`. A ref may
// contain `/` (`feature/x`), so it is encoded segment by segment, never as
// one piece; the server (and the resolve API) decide where the ref ends.

/** `a/b c` → `a/b%20c`: each segment encoded once. */
export function encodePath(path: string): string {
  return path === '' ? '' : path.split('/').map(encodeURIComponent).join('/');
}

const base = (project: string) => `/${encodePath(project)}`;
const withPath = (prefix: string, path: string) =>
  path === '' ? prefix : `${prefix}/${encodePath(path)}`;

export const treeUrl = (project: string, ref: string, path = '') =>
  withPath(`${base(project)}/-/tree/${encodePath(ref)}`, path);

export const blobUrl = (project: string, ref: string, path: string) =>
  withPath(`${base(project)}/-/blob/${encodePath(ref)}`, path);

export const blameUrl = (project: string, ref: string, path: string) =>
  withPath(`${base(project)}/-/blame/${encodePath(ref)}`, path);

export const commitsUrl = (project: string, ref: string, path = '') =>
  withPath(`${base(project)}/-/commits/${encodePath(ref)}`, path);

export const commitUrl = (project: string, oid: string) => `${base(project)}/-/commit/${oid}`;

export const branchesUrl = (project: string) => `${base(project)}/-/branches`;
export const tagsUrl = (project: string) => `${base(project)}/-/tags`;

/** `from...to`: git does not allow `..` in a ref name, so `...` is a safe separator. */
export const compareUrl = (project: string, from?: string, to?: string) =>
  from === undefined || to === undefined
    ? `${base(project)}/-/compare`
    : `${base(project)}/-/compare/${encodePath(from)}...${encodePath(to)}`;

/** The merge request list (ADR-0015 amendment B0: `pulls`); `state` and
 * `before` (the previous page's `next_before`) are the query. */
export const pullsUrl = (project: string, query?: { state: string; before?: number }) => {
  if (query === undefined) return `${base(project)}/-/pulls`;
  const q = new URLSearchParams({ state: query.state });
  if (query.before !== undefined) q.set('before', String(query.before));
  return `${base(project)}/-/pulls?${q.toString()}`;
};

export const newPullUrl = (project: string) => `${base(project)}/-/pulls/new`;

/** One merge request; `changes`: its diff tab. */
export const pullUrl = (project: string, iid: number, tab?: 'changes') =>
  tab === undefined ? `${base(project)}/-/pull/${iid}` : `${base(project)}/-/pull/${iid}/${tab}`;

/** Server routes (not screens): plain links, never router links. */
export const rawUrl = (project: string, ref: string, path: string) =>
  withPath(`${base(project)}/-/raw/${encodePath(ref)}`, path);

export const archiveUrl = (project: string, ref: string, format: 'zip' | 'tar.gz') =>
  `${base(project)}/-/archive/${encodePath(ref)}.${format}`;

/** The screen words that take `{ref}[/{path}]` as their tail. */
export const SPEC_VIEWS = ['tree', 'blob', 'blame', 'commits'] as const;

export function parentPath(path: string): string {
  const i = path.lastIndexOf('/');
  return i < 0 ? '' : path.slice(0, i);
}

export function baseName(path: string): string {
  const i = path.lastIndexOf('/');
  return i < 0 ? path : path.slice(i + 1);
}

/** Lower-case extension without the dot (`README.MD` → `md`), '' if none. */
export function extensionOf(path: string): string {
  const name = baseName(path);
  const i = name.lastIndexOf('.');
  return i <= 0 ? '' : name.slice(i + 1).toLowerCase();
}

/** Images the browser may show from the raw route (ADR-0015: png / jpeg /
 * gif / webp only; SVG is a download). */
export const IMAGE_EXTENSIONS = ['png', 'jpg', 'jpeg', 'gif', 'webp'] as const;

export function isImagePath(path: string): boolean {
  return (IMAGE_EXTENSIONS as readonly string[]).includes(extensionOf(path));
}

/** Formats the screen never draws (ADR-0015 amendment: download only). */
const DOWNLOAD_ONLY = [
  'svg', 'svgz', 'html', 'htm', 'xhtml', 'xht', 'shtml', 'mht', 'mhtml', 'xml', 'xsl', 'xslt', 'pdf',
];

export function isDownloadOnly(path: string): boolean {
  return DOWNLOAD_ONLY.includes(extensionOf(path));
}

export function isMarkdownPath(path: string): boolean {
  return ['md', 'markdown', 'mdown'].includes(extensionOf(path));
}

/** The document list (plan 13 stage 3); `ref` (a branch, tag or commit) and
 * `after` (the previous page's `next_after`) are the query. */
export const documentsUrl = (project: string, query?: { ref?: string; after?: string }) => {
  const q = new URLSearchParams();
  if (query?.ref !== undefined) q.set('ref', query.ref);
  if (query?.after !== undefined) q.set('after', query.after);
  const s = q.toString();
  return `${base(project)}/-/documents${s === '' ? '' : `?${s}`}`;
};

/** One document by its id; `ref` and `lang` (a translation) are the query,
 * `block` the fragment of a block's card. */
export const documentUrl = (
  project: string,
  id: string,
  query?: { ref?: string; lang?: string; block?: string },
) => {
  const q = new URLSearchParams();
  if (query?.ref !== undefined) q.set('ref', query.ref);
  if (query?.lang !== undefined) q.set('lang', query.lang);
  const s = q.toString();
  const hash = query?.block === undefined ? '' : `#${blockAnchor(query.block)}`;
  return `${base(project)}/-/documents/${encodeURIComponent(id)}${s === '' ? '' : `?${s}`}${hash}`;
};

/** The element id of a block's card (the `user-content-` prefix keeps
 * document ids from clobbering the page's globals, as Markdown headings). */
export const blockAnchor = (block: string) => `user-content-block-${block}`;

/** The file shown as a folder's README (`README`, `readme.md`, …). */
export function isReadmeName(name: string): boolean {
  return /^readme(\.(md|markdown|txt))?$/i.test(name);
}
