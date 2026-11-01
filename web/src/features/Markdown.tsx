// Markdown, drawn so that nothing in the document can run (ADR-0022 §7).
//
// - react-markdown builds React elements from the syntax tree; it never
//   writes HTML strings, and `rehype-raw` is not installed: a raw HTML block
//   or inline tag in the source is dropped (`skipHtml`), not escaped into
//   view and not executed. No dangerouslySetInnerHTML anywhere (eslint).
// - Links: http, https, mailto and relative paths only. Everything else
//   (javascript:, data:, vbscript:, file:, a scheme-relative `//host`, any
//   control character or backslash) is removed by `urlTransform` before a
//   component sees it.
// - Images: only this repository's own png / jpeg / gif / webp, through the
//   raw route (same origin; CSP img-src 'self'). An external or data: image
//   is not drawn; the reader gets its text, and an external one as a link.
// - Heading ids carry the `user-content-` prefix (no DOM clobbering of
//   `window.name` and the like); fragment links are mapped to it.
// - Code, tables and the like scroll inside a focusable region.
import type { ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import ReactMarkdown, { type Components } from 'react-markdown';
import { Link } from 'react-router';
import remarkGfm from 'remark-gfm';

import { blobUrl, isImagePath, rawUrl, treeUrl } from './repoUrls';
import styles from './Markdown.module.css';

const ID_PREFIX = 'user-content-';
const ALLOWED_SCHEMES = ['http:', 'https:', 'mailto:'];
// eslint-disable-next-line no-control-regex
const CONTROL = /[\u0000-\u001f\u007f-\u009f]/;

/** Keep a URL only if it is http(s), mailto, a fragment or a relative path. */
export function safeUrl(raw: string): string | null {
  const url = raw.trim();
  if (url === '' || CONTROL.test(url) || url.includes('\\')) return null;
  if (url.startsWith('//')) return null;
  const scheme = /^([a-zA-Z][a-zA-Z0-9+.-]*):/.exec(url);
  if (scheme !== null) {
    return ALLOWED_SCHEMES.includes(`${(scheme[1] ?? '').toLowerCase()}:`) ? url : null;
  }
  return url;
}

function decode(segment: string): string | null {
  try {
    const out = decodeURIComponent(segment);
    return out.includes('/') || out.includes('\0') ? null : out;
  } catch {
    return null;
  }
}

/** A link's path against the directory of the document: `null` when it
 * climbs out of the repository or does not decode. `/x` is the repo root. */
export function resolveRepoPath(dir: string, target: string): string | null {
  const out = target.startsWith('/') || dir === '' ? [] : dir.split('/');
  for (const raw of target.split('/')) {
    if (raw === '' || raw === '.') continue;
    if (raw === '..') {
      if (out.length === 0) return null;
      out.pop();
      continue;
    }
    const seg = decode(raw);
    if (seg === null || seg === '..') return null;
    out.push(seg);
  }
  return out.join('/');
}

function textOf(node: ReactNode): string {
  if (typeof node === 'string' || typeof node === 'number') return String(node);
  if (Array.isArray(node)) return node.map(textOf).join('');
  if (typeof node === 'object' && node !== null && 'props' in node) {
    const props = node.props as { children?: ReactNode };
    return textOf(props.children);
  }
  return '';
}

export function headingId(text: string): string {
  const slug = text
    .trim()
    .toLowerCase()
    .replace(/[^\p{L}\p{N}\s_-]/gu, '')
    .replace(/\s+/gu, '-');
  return `${ID_PREFIX}${slug}`;
}

export interface MarkdownProps {
  source: string;
  /** Path of the project (`acme/web`): the base of relative links and images. */
  project: string;
  /** The ref name as the screens spell it (`main`). */
  refName: string;
  /** Directory of the document in the repository (`docs`, '' for the root). */
  dir: string;
  /** What the raw addresses of images use (the commit id: exact); default `refName`. */
  rawRef?: string;
  /** Added to every heading level (the page already has its own h1 / h2). */
  headingOffset?: number;
  /** Where a link to a file or folder of the project goes, instead of the
   * file screens (it served the removed document portal, ADR-0023; unused now).
   * `null`: no link, the text only. */
  linkTo?: (path: string, folder: boolean) => string | null;
}

export function Markdown({
  source,
  project,
  refName,
  dir,
  headingOffset = 0,
  rawRef,
  linkTo,
}: MarkdownProps) {
  const { t } = useTranslation();

  const components: Components = {
    a({ href, children }) {
      if (href === undefined) return <span>{children}</span>;
      if (href.startsWith('#')) {
        const id = href.slice(1);
        const target = id.startsWith(ID_PREFIX) ? id : `${ID_PREFIX}${id}`;
        return <a href={`#${target}`}>{children}</a>;
      }
      if (/^[a-zA-Z][a-zA-Z0-9+.-]*:/.test(href)) {
        return (
          <a href={href} rel="noopener noreferrer">
            {children}
          </a>
        );
      }
      const [pathPart = '', ...rest] = href.split('#');
      const bare = pathPart.split('?')[0] ?? '';
      const resolved = resolveRepoPath(dir, bare);
      if (resolved === null) return <span>{children}</span>;
      const fragment = rest.length > 0 ? `#${rest.join('#')}` : '';
      const folder = bare.endsWith('/') || resolved === '';
      const to =
        linkTo !== undefined
          ? linkTo(resolved, folder)
          : folder
            ? treeUrl(project, refName, resolved)
            : blobUrl(project, refName, resolved);
      if (to === null) return <span>{children}</span>;
      return <Link to={`${to}${fragment}`}>{children}</Link>;
    },
    img({ src, alt }) {
      const label = alt ?? '';
      if (typeof src !== 'string') return <span>{label}</span>;
      if (/^[a-zA-Z][a-zA-Z0-9+.-]*:/.test(src)) {
        // External: never fetched by the page, offered as a link.
        return (
          <a href={src} rel="noopener noreferrer">
            {label === '' ? t('markdown.externalImage') : label}
          </a>
        );
      }
      const resolved = resolveRepoPath(dir, src.split('#')[0]?.split('?')[0] ?? '');
      if (resolved === null || resolved === '' || !isImagePath(resolved)) {
        return <span>{label}</span>;
      }
      return <img src={rawUrl(project, rawRef ?? refName, resolved)} alt={label} loading="lazy" />;
    },
    input({ checked }) {
      return (
        <input
          type="checkbox"
          checked={checked === true}
          disabled
          readOnly
          aria-label={t('markdown.task')}
        />
      );
    },
    pre({ children }) {
      return (
        <div className={styles.scroll} role="region" aria-label={t('markdown.code')} tabIndex={0}>
          <pre>{children}</pre>
        </div>
      );
    },
    table({ children }) {
      return (
        <div className={styles.scroll} role="region" aria-label={t('markdown.table')} tabIndex={0}>
          <table>{children}</table>
        </div>
      );
    },
  };
  const levels = ['h1', 'h2', 'h3', 'h4', 'h5', 'h6'] as const;
  for (const [index, level] of levels.entries()) {
    components[level] = ({ children }) => {
      const Tag = levels[Math.min(index + headingOffset, levels.length - 1)] ?? level;
      return <Tag id={headingId(textOf(children))}>{children}</Tag>;
    };
  }

  return (
    <div className={styles.markdown} data-testid="markdown">
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        skipHtml
        urlTransform={safeUrl}
        components={components}
      >
        {source}
      </ReactMarkdown>
    </div>
  );
}
