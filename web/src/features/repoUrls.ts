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
