// One quick note (plan 20 N2, ADR-0040 §6): the same file `gstn note new`
// writes (forge-doc `note::quick_note`), built in the browser so the web and
// the CLI agree byte for byte. Pure: values in, text out.
//
// - The id is `N-YYYYMMDD-HHMMSS` in UTC; the file is `<id>.md` at the
//   repository root (the lint wants the id to be the file's stem).
// - The title and the owner are written as YAML double-quoted scalars, so
//   nothing typed changes what the front matter means.

/** The longest title written into a note (characters; longer is cut). */
export const MAX_TITLE_CHARS = 200;
/** The page type every note has (`.forge/doctypes/note.yml`). */
export const NOTE_DOCTYPE = 'note';

const NOTE_ID = /^N-\d{8}-\d{6}$/;

const two = (n: number) => String(n).padStart(2, '0');

/** The note id of `date` (UTC), `null` outside the years 1000-9999. */
export function noteId(date: Date): string | null {
  const y = date.getUTCFullYear();
  if (!Number.isFinite(y) || y < 1000 || y > 9999) return null;
  return (
    `N-${String(y)}${two(date.getUTCMonth() + 1)}${two(date.getUTCDate())}` +
    `-${two(date.getUTCHours())}${two(date.getUTCMinutes())}${two(date.getUTCSeconds())}`
  );
}

export function isNoteId(id: string): boolean {
  return NOTE_ID.test(id);
}

/** The file of note `id`. */
export function notePath(id: string): string {
  return `${id}.md`;
}

/** The note id of a file at the root (`N-….md`), else `null`. */
export function noteIdOfPath(path: string): string | null {
  if (path.includes('/') || !path.endsWith('.md')) return null;
  const id = path.slice(0, -'.md'.length);
  return isNoteId(id) ? id : null;
}

/** Characters as the server counts them (code points). */
function cut(text: string, max: number): string {
  return Array.from(text).slice(0, max).join('');
}

/** The title of a note typed without one: the body's first non-empty line,
 * without leading `#`, cut at [`MAX_TITLE_CHARS`]; `id` for an empty body. */
export function defaultTitle(body: string, id: string): string {
  for (const raw of body.split(/\r?\n/)) {
    const line = raw.trim().replace(/^#+/, '').trim();
    if (line !== '') return cut(line, MAX_TITLE_CHARS);
  }
  return id;
}

function isEscaped(code: number): boolean {
  return (
    code < 0x20 ||
    (code >= 0x7f && code <= 0x9f) ||
    code === 0x2028 ||
    code === 0x2029 ||
    code === 0xfeff ||
    code === 0xfffe ||
    code === 0xffff
  );
}

/** `s` as a YAML double-quoted scalar (as forge-doc writes it). */
export function quoted(s: string): string {
  let out = '"';
  for (const c of s) {
    const code = c.codePointAt(0) ?? 0;
    if (c === '"') out += '\\"';
    else if (c === '\\') out += '\\\\';
    else if (isEscaped(code)) out += `\\u${code.toString(16).padStart(4, '0')}`;
    else out += c;
  }
  return `${out}"`;
}

/** One note: front matter (`doctype: note`, id, title, owner,
 * `classification: internal`) and the body with one trailing newline. */
export function quickNote(id: string, title: string, owner: string, body: string): string {
  const front = [
    '---',
    `doctype: ${NOTE_DOCTYPE}`,
    `id: ${id}`,
    `title: ${quoted(cut(title, MAX_TITLE_CHARS))}`,
    `owner: ${quoted(owner)}`,
    'classification: internal',
    '---',
    '',
    '',
  ].join('\n');
  return `${front}${body.replace(/[\r\n]+$/, '')}\n`;
}
