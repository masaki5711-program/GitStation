// The simple editor (E2, docs/api/single-file-commit.md): one text file of a
// branch, edited in the browser and committed like a push of one commit.
//
// - `/-/edit/{ref}/{path}`: an existing file; `/-/new/{ref}[/{dir}]`: a new
//   file in a folder (the path is typed here).
// - Two panes of equal weight (Edit | Preview) on a wide screen; below 48rem
//   tabs. The preview is the same Markdown renderer as the file screen.
// - "Save" (or Ctrl+S / Cmd+S) opens the save dialog; the commit goes to a
//   new branch with a merge request by default, or to the branch itself.
// - Unsaved text is guarded: reload / close (beforeunload), in-app links and
//   Back (useLeaveGuard).
// The file is read at the resolved commit, and that commit is `start_sha`: a
// file someone changed since is refused by the server (FG-GIT-0027), never
// overwritten.
import { useCallback, useDeferredValue, useEffect, useId, useRef, useState, useSyncExternalStore } from 'react';
import { useTranslation } from 'react-i18next';
import { Link, useNavigate } from 'react-router';

import { MAX_CONTENT_BYTES, type SingleFileCommitResult } from '../../../api/fileCommits';
import { useBlob } from '../../../api/repo';
import type { Rev } from '../../../api/types';
import { Loading } from '../../../components/Loading';
import controls from '../../../components/controls.module.css';
import { cleanPath, countText, pathProblem } from '../../../features/editorModel';
import { MarkdownView } from '../../../features/MarkdownView';
import { formatBytes } from '../../../features/labels';
import { baseName, blobUrl, isMarkdownPath, parentPath, pullUrl } from '../../../features/repoUrls';
import { useDocumentTitle } from '../../../hooks/useDocumentTitle';
import { useLeaveGuard } from '../../../hooks/useLeaveGuard';
import { PageHeader } from '../../../ui/PageHeader';
import { ConfirmDialog } from '../../settings/common';
import { PathCrumbs, RepoError, WithSpec } from '../common';
import { SaveDialog } from './SaveDialog';
import { savedState } from './SavedNotice';
import styles from './Editor.module.css';

export type EditorMode = 'edit' | 'new';

/** Below this width the panes become tabs (the same 48rem as the CSS). */
export const EDITOR_NARROW_QUERY = '(max-width: 47.99rem)';

function useNarrowEditor(): boolean {
  return useSyncExternalStore(
    (onChange) => {
      if (typeof window.matchMedia !== 'function') return () => undefined;
      const m = window.matchMedia(EDITOR_NARROW_QUERY);
      m.addEventListener('change', onChange);
      return () => {
        m.removeEventListener('change', onChange);
      };
    },
    () => (typeof window.matchMedia === 'function' ? window.matchMedia(EDITOR_NARROW_QUERY).matches : false),
    () => false,
  );
}

export function EditorScreen({
  projectId,
  project,
  spec,
  mode,
  role,
  isAdmin,
}: {
  projectId: number;
  project: string;
  spec: string;
  mode: EditorMode;
  role: string | null;
  isAdmin: boolean;
}) {
  return (
    <WithSpec projectId={projectId} project={project} spec={spec}>
      {(s) =>
        s.rev.type !== 'branch' || s.rev.full_name === null ? (
          <NotABranch project={project} rev={s.rev} path={s.path} />
        ) : mode === 'edit' ? (
          <ExistingFile
            key={s.rev.commit}
            projectId={projectId}
            project={project}
            rev={s.rev}
            path={s.path}
            role={role}
            isAdmin={isAdmin}
          />
        ) : (
          <Editor
            key={s.rev.commit}
            projectId={projectId}
            project={project}
            rev={s.rev}
            mode="new"
            initialPath={s.path === '' ? '' : `${s.path}/`}
            initialText=""
            role={role}
            isAdmin={isAdmin}
          />
        )
      }
    </WithSpec>
  );
}

/** A tag or a commit cannot take a commit: say so, with the way back. */
function NotABranch({ project, rev, path }: { project: string; rev: Rev; path: string }) {
  const { t } = useTranslation();
  useDocumentTitle(t('editor.notBranchTitle'));
  return (
    <>
      <PageHeader title={t('editor.notBranchTitle')} subtitle={t('editor.notBranchBody')} />
      {path !== '' && (
        <p>
          <Link className={controls.button} to={blobUrl(project, rev.name, path)}>
            {t('editor.backToFile')}
          </Link>
        </p>
      )}
    </>
  );
}

function ExistingFile({
  projectId,
  project,
  rev,
  path,
  role,
  isAdmin,
}: {
  projectId: number;
  project: string;
  rev: Rev;
  path: string;
  role: string | null;
  isAdmin: boolean;
}) {
  const { t, i18n } = useTranslation();
  // Read at the commit the address resolved to: that commit is start_sha.
  const blob = useBlob(projectId, rev.commit, path, path !== '');
  useDocumentTitle(path === '' ? null : t('editor.docTitle', { name: baseName(path) }));
  if (path === '') return <NotABranch project={project} rev={rev} path={path} />;
  if (blob.isPending) return <Loading />;
  if (blob.isError) {
    return (
      <RepoError
        error={blob.error}
        onRetry={() => {
          void blob.refetch();
        }}
      />
    );
  }
  const file = blob.data;
  const reason =
    file.type !== 'blob' || file.binary || file.content === null
      ? t('editor.cannotEditBinary')
      : file.truncated || file.size > MAX_CONTENT_BYTES
        ? t('editor.cannotEditLarge', { size: formatBytes(MAX_CONTENT_BYTES, i18n.language) })
        : null;
  if (reason !== null || file.content === null) {
    return (
      <>
        <PathCrumbs project={project} rev={rev.name} path={path} />
        <PageHeader title={t('editor.cannotEditTitle')} subtitle={reason ?? undefined} />
        <p>
          <Link className={controls.button} to={blobUrl(project, rev.name, path)}>
            {t('editor.backToFile')}
          </Link>
        </p>
      </>
    );
  }
  return (
    <Editor
      projectId={projectId}
      project={project}
      rev={rev}
      mode="edit"
      initialPath={path}
      initialText={file.content}
      role={role}
      isAdmin={isAdmin}
    />
  );
}

function Editor({
  projectId,
  project,
  rev,
  mode,
  initialPath,
  initialText,
  role,
  isAdmin,
}: {
  projectId: number;
  project: string;
  rev: Rev;
  mode: EditorMode;
  initialPath: string;
  initialText: string;
  role: string | null;
  isAdmin: boolean;
}) {
  const { t, i18n } = useTranslation();
  const navigate = useNavigate();
  const narrow = useNarrowEditor();
  const ids = useId();
  const pathInput = useRef<HTMLInputElement>(null);
  const [text, setText] = useState(initialText);
  const [pathText, setPathText] = useState(initialPath);
  const [pathError, setPathError] = useState<string | null>(null);
  const [tab, setTab] = useState<'edit' | 'preview'>('edit');
  const [saving, setSaving] = useState(false);
  const [dialog, setDialog] = useState(false);
  const saveButton = useRef<HTMLButtonElement>(null);
  useDocumentTitle(
    mode === 'edit' ? t('editor.docTitle', { name: baseName(initialPath) }) : t('editor.newTitle'),
  );

  const path = mode === 'edit' ? initialPath : cleanPath(pathText);
  const dirty = text !== initialText || (mode === 'new' && pathText !== initialPath);
  const counts = countText(text);
  const tooLarge = counts.bytes > MAX_CONTENT_BYTES;
  const deferred = useDeferredValue(text);
  const showPreview = isMarkdownPath(path) || (mode === 'new' && baseName(path) === '');

  // Reload / close while unsaved: the browser asks.
  useEffect(() => {
    if (!dirty || saving) return;
    const warn = (e: BeforeUnloadEvent) => {
      e.preventDefault();
    };
    window.addEventListener('beforeunload', warn);
    return () => {
      window.removeEventListener('beforeunload', warn);
    };
  }, [dirty, saving]);
  const leave = useLeaveGuard(dirty && !saving, () => undefined);

  const openSave = useCallback(() => {
    if (mode === 'new') {
      const problem = pathProblem(path);
      if (problem !== null) {
        setPathError(problem === 'empty' ? t('editor.pathRequired') : t('editor.pathIsFolder'));
        pathInput.current?.focus();
        return;
      }
    }
    setPathError(null);
    setDialog(true);
  }, [mode, path, t]);

  // Ctrl+S / Cmd+S: the save dialog, not the browser's "save page".
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.ctrlKey || e.metaKey) && !e.altKey && e.key.toLowerCase() === 's') {
        e.preventDefault();
        if (!dialog) openSave();
      }
    };
    window.addEventListener('keydown', onKey);
    return () => {
      window.removeEventListener('keydown', onKey);
    };
  }, [dialog, openSave]);

  const onSaved = (result: SingleFileCommitResult) => {
    setSaving(true);
    setDialog(false);
    const mr = result.merge_request ?? null;
    const to = mr !== null ? pullUrl(project, mr.iid) : blobUrl(project, result.branch, result.path);
    // replace: the guard's sentinel entry (if any) is not left behind.
    void navigate(to, { replace: true, state: savedState(result) });
  };

  const title = mode === 'edit' ? path : t('editor.newTitle');
  const status = dirty ? t('editor.statusUnsaved') : mode === 'new' ? t('editor.statusEmpty') : t('editor.statusSaved');
  const editPanelId = `${ids}-edit`;
  const previewPanelId = `${ids}-preview`;
  const showEdit = !narrow || !showPreview || tab === 'edit';
  const showPrev = showPreview && (!narrow || tab === 'preview');

  const editorPane = (
    <section
      className={styles.pane}
      aria-labelledby={`${editPanelId}-title`}
      {...(narrow && showPreview ? { id: editPanelId, role: 'tabpanel' } : {})}
      hidden={!showEdit}
    >
      <h2 id={`${editPanelId}-title`} className={narrow && showPreview ? styles.srOnly : styles.paneTitle}>
        {t('editor.editPane')}
      </h2>
      <label htmlFor={`${ids}-text`} className={styles.srOnly}>
        {t('editor.contentLabel', { path: path === '' ? t('editor.newTitle') : path })}
      </label>
      <textarea
        id={`${ids}-text`}
        className={styles.textarea}
        value={text}
        spellCheck={false}
        autoCapitalize="off"
        autoComplete="off"
        aria-describedby={`${ids}-counts`}
        onChange={(e) => {
          setText(e.target.value);
        }}
      />
    </section>
  );
  const previewPane = showPreview ? (
    <section
      className={styles.pane}
      aria-labelledby={`${previewPanelId}-title`}
      {...(narrow ? { id: previewPanelId, role: 'tabpanel' } : {})}
      hidden={!showPrev}
      data-testid="editor-preview"
    >
      <h2 id={`${previewPanelId}-title`} className={narrow ? styles.srOnly : styles.paneTitle}>
        {t('editor.previewPane')}
      </h2>
      <div className={styles.preview}>
        {deferred.trim() === '' ? (
          <p className={styles.previewEmpty}>{t('editor.previewEmpty')}</p>
        ) : (
          <MarkdownView
            source={deferred}
            project={project}
            refName={rev.name}
            rawRef={rev.commit}
            dir={parentPath(path)}
            headingOffset={2}
          />
        )}
      </div>
    </section>
  ) : null;

  return (
    <>
      <PathCrumbs project={project} rev={rev.name} path={mode === 'edit' ? path : parentPath(initialPath)} />
      <PageHeader
        title={<span className={styles.title}>{title}</span>}
        subtitle={t('editor.subtitle', { branch: rev.name })}
        actions={
          <button ref={saveButton} type="button" className={controls.primary} onClick={openSave}>
            {t('editor.save')}
          </button>
        }
      >
        <p className={styles.status} role="status" data-dirty={dirty ? 'true' : 'false'}>
          <span className={styles.statusDot} aria-hidden="true" />
          {status}
        </p>
      </PageHeader>

      {mode === 'new' && (
        <div className={`${controls.field} ${styles.pathField}`}>
          <label htmlFor={`${ids}-path`} className={controls.label}>
            {t('editor.pathLabel')}
          </label>
          <input
            ref={pathInput}
            id={`${ids}-path`}
            className={`${controls.input} ${styles.pathInput}`}
            value={pathText}
            placeholder={t('editor.pathPlaceholder')}
            spellCheck={false}
            autoCapitalize="off"
            autoComplete="off"
            required
            aria-invalid={pathError !== null ? true : undefined}
            aria-describedby={pathError !== null ? `${ids}-path-error` : `${ids}-path-help`}
            onChange={(e) => {
              setPathText(e.target.value);
              setPathError(null);
            }}
          />
          {pathError !== null ? (
            <p id={`${ids}-path-error`} className={controls.fieldError}>
              {pathError}
            </p>
          ) : (
            <p id={`${ids}-path-help`} className={controls.help}>
              {t('editor.pathHelp')}
            </p>
          )}
        </div>
      )}

      {narrow && showPreview && (
        <div role="tablist" aria-label={t('editor.tabsLabel')} className={styles.tabs}>
          {(['edit', 'preview'] as const).map((which) => (
            <button
              key={which}
              type="button"
              role="tab"
              className={styles.tab}
              aria-selected={tab === which}
              aria-controls={which === 'edit' ? editPanelId : previewPanelId}
              tabIndex={tab === which ? 0 : -1}
              onClick={() => {
                setTab(which);
              }}
              onKeyDown={(e) => {
                if (e.key === 'ArrowLeft' || e.key === 'ArrowRight') {
                  e.preventDefault();
                  const next = which === 'edit' ? 'preview' : 'edit';
                  setTab(next);
                  const list = e.currentTarget.parentElement;
                  const other = list?.querySelector<HTMLButtonElement>(`[aria-controls="${next === 'edit' ? editPanelId : previewPanelId}"]`);
                  other?.focus();
                }
              }}
            >
              {which === 'edit' ? t('editor.editPane') : t('editor.previewPane')}
            </button>
          ))}
        </div>
      )}

      <div className={showPreview && !narrow ? styles.split : styles.single}>
        {editorPane}
        {previewPane}
      </div>

      <p id={`${ids}-counts`} className={styles.counts}>
        <span>{t('editor.counts', { lines: counts.lines.toLocaleString(i18n.language), chars: counts.chars.toLocaleString(i18n.language) })}</span>
        <span>{formatBytes(counts.bytes, i18n.language)}</span>
        <span className={styles.hintKey}>{t('editor.shortcutHint')}</span>
      </p>
      {tooLarge && (
        <p className={styles.warning} role="status">
          {t('editor.tooLarge', { size: formatBytes(MAX_CONTENT_BYTES, i18n.language) })}
        </p>
      )}

      <SaveDialog
        open={dialog}
        projectId={projectId}
        project={project}
        rev={rev}
        mode={mode}
        path={path}
        text={text}
        role={role}
        isAdmin={isAdmin}
        onCancel={() => {
          setDialog(false);
          saveButton.current?.focus();
        }}
        onSaved={onSaved}
      />
      <ConfirmDialog
        open={leave.target !== null}
        title={t('editor.leaveTitle')}
        message={<p>{t('editor.leaveBody')}</p>}
        confirmLabel={t('editor.leaveConfirm')}
        busy={false}
        error={null}
        onConfirm={leave.proceed}
        onCancel={leave.stay}
      />
    </>
  );
}
