// The organisation selectors of an approver list (ADR-0038 §3.1): whose unit
// (the requester's section, the requester's department, or a chosen unit),
// who in it (anyone, a position rank of at least N, or position codes) and
// whether to look one level up when no one is there. Used by the approval
// rule dialog and the deploy approvers dialog; the server checks again.
import { useRef, useState, type KeyboardEvent } from 'react';
import { useTranslation } from 'react-i18next';

import { useOrgUnitSearch, type ApproverSelector } from '../../api/approvers';
import { SelectField, TextField } from '../../components/Field';
import { Loading } from '../../components/Loading';
import { QueryError } from '../../components/QueryError';
import controls from '../../components/controls.module.css';
import { selectorText } from '../../features/approvers';
import styles from './Settings.module.css';

/** The most selectors one list holds (ADR-0038 §3.4). */
export const MAX_SELECTORS = 5;

type Kind = 'anyone' | 'rank' | 'positions';

const POSITION = /^[a-z][a-z0-9_]{0,63}$/;

function Check({ label, checked, onChange }: { label: string; checked: boolean; onChange: (v: boolean) => void }) {
  return (
    <div className={controls.field}>
      <label className={controls.choiceLabel}>
        <input
          type="checkbox"
          checked={checked}
          onChange={(e) => {
            onChange(e.target.checked);
          }}
        />
        <span>{label}</span>
      </label>
    </div>
  );
}

/** Search the org master's copy and choose one unit. */
function UnitPicker({ chosen, onChoose }: { chosen: string; onChoose: (code: string) => void }) {
  const { t, i18n } = useTranslation();
  const [text, setText] = useState('');
  const [term, setTerm] = useState('');
  const found = useOrgUnitSearch(term);
  const search = () => {
    const s = text.trim();
    if (s !== '') setTerm(s);
  };
  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    if (e.key === 'Enter' && e.target instanceof HTMLInputElement) {
      e.preventDefault();
      search();
    }
  };
  const name = (u: { name_ja: string; name_en?: string | null; name_id?: string | null }) =>
    i18n.language.startsWith('en')
      ? (u.name_en ?? u.name_ja)
      : i18n.language.startsWith('id')
        ? (u.name_id ?? u.name_en ?? u.name_ja)
        : u.name_ja;
  return (
    <div onKeyDown={onKeyDown}>
      <p className={styles.muted}>
        {chosen === '' ? t('approvers.editor.noUnit') : t('approvers.editor.chosenUnit', { unit: chosen })}
      </p>
      <TextField
        label={t('approvers.editor.unitSearch')}
        value={text}
        onChange={setText}
        help={t('approvers.editor.unitSearchHelp')}
      />
      <button type="button" className={controls.button} onClick={search}>
        {t('approvers.editor.unitSearchButton')}
      </button>
      {found.isPending && term !== '' && <Loading />}
      {found.isError && <QueryError error={found.error} />}
      <p role="status" className={styles.muted}>
        {found.isSuccess && found.data.items.length === 0 ? t('approvers.editor.noUnitMatch') : ''}
      </p>
      {found.isSuccess && found.data.items.length > 0 && (
        <ul className={styles.results} aria-label={t('approvers.editor.unitResults')}>
          {found.data.items.map((u) => (
            <li key={`${u.plant_code}-${u.code}`}>
              <span>
                {name(u)} <code>{u.code}</code> <span className={styles.muted}>{u.level}</span>
              </span>
              <button
                type="button"
                className={controls.button}
                aria-label={t('approvers.editor.chooseUnitOf', { name: name(u) })}
                onClick={() => {
                  onChoose(u.code);
                }}
              >
                {t('approvers.editor.chooseUnit')}
              </button>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

/** The selectors chosen, and a form adding one. */
export function SelectorEditor({
  selectors,
  onChange,
}: {
  selectors: ApproverSelector[];
  onChange: (next: ApproverSelector[]) => void;
}) {
  const { t } = useTranslation();
  const box = useRef<HTMLFieldSetElement>(null);
  const [relation, setRelation] = useState<ApproverSelector['relation']>('requester_section');
  const [unit, setUnit] = useState('');
  const [wide, setWide] = useState(false);
  const [kind, setKind] = useState<Kind>('rank');
  const [rank, setRank] = useState('55');
  const [positions, setPositions] = useState('');
  const [escalate, setEscalate] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);

  const full = selectors.length >= MAX_SELECTORS;
  const add = () => {
    if (full) return;
    const s: ApproverSelector = { relation };
    if (relation === 'unit') {
      if (unit === '') {
        setProblem(t('approvers.editor.unitRequired'));
        return;
      }
      s.unit = unit;
    }
    if (wide && relation !== 'requester_section') s.include_descendants = true;
    if (kind === 'rank') {
      const r = rank.trim();
      if (!/^[0-9]{1,3}$/.test(r)) {
        setProblem(t('approvers.editor.rankInvalid'));
        return;
      }
      s.min_rank = Number(r);
    } else if (kind === 'positions') {
      const list = positions
        .split(',')
        .map((p) => p.trim())
        .filter((p) => p !== '');
      if (list.length === 0 || list.length > 10 || !list.every((p) => POSITION.test(p))) {
        setProblem(t('approvers.editor.positionsInvalid'));
        return;
      }
      s.positions = list;
    }
    if (escalate) s.escalate = 'parent';
    setProblem(null);
    onChange([...selectors, s]);
    setUnit('');
    box.current?.querySelector('select')?.focus();
  };

  return (
    <fieldset ref={box} className={controls.field}>
      <legend className={controls.label}>{t('approvers.editor.title')}</legend>
      <p className={styles.hint}>{t('approvers.editor.explain')}</p>
      {selectors.length > 0 && (
        <ul className={styles.results} aria-label={t('approvers.editor.chosen')}>
          {selectors.map((s, i) => {
            const text = selectorText(t, s);
            return (
              <li key={`${String(i)}-${text}`}>
                <span>{text}</span>
                <button
                  type="button"
                  className={controls.button}
                  aria-label={t('approvers.editor.removeOf', { what: text })}
                  onClick={() => {
                    onChange(selectors.filter((_, j) => j !== i));
                    box.current?.querySelector('select')?.focus();
                  }}
                >
                  {t('approvers.editor.remove')}
                </button>
              </li>
            );
          })}
        </ul>
      )}
      {full ? (
        <p className={styles.muted}>{t('approvers.editor.full', { max: MAX_SELECTORS })}</p>
      ) : (
        <>
          <SelectField
            label={t('approvers.editor.relation')}
            value={relation}
            onChange={(v) => {
              if (v === 'requester_section' || v === 'requester_department' || v === 'unit') setRelation(v);
            }}
          >
            <option value="requester_section">{t('approvers.editor.relationSection')}</option>
            <option value="requester_department">{t('approvers.editor.relationDepartment')}</option>
            <option value="unit">{t('approvers.editor.relationUnit')}</option>
          </SelectField>
          {relation === 'unit' && <UnitPicker chosen={unit} onChoose={setUnit} />}
          {relation !== 'requester_section' && (
            <Check label={t('approvers.editor.wide')} checked={wide} onChange={setWide} />
          )}
          <SelectField
            label={t('approvers.editor.who')}
            value={kind}
            onChange={(v) => {
              if (v === 'anyone' || v === 'rank' || v === 'positions') setKind(v);
            }}
          >
            <option value="rank">{t('approvers.editor.whoRank')}</option>
            <option value="positions">{t('approvers.editor.whoPositions')}</option>
            <option value="anyone">{t('approvers.editor.whoAnyone')}</option>
          </SelectField>
          {kind === 'rank' && (
            <TextField
              label={t('approvers.editor.rank')}
              value={rank}
              onChange={setRank}
              help={t('approvers.editor.rankHelp')}
            />
          )}
          {kind === 'positions' && (
            <TextField
              label={t('approvers.editor.positions')}
              value={positions}
              onChange={setPositions}
              help={t('approvers.editor.positionsHelp')}
            />
          )}
          <Check label={t('approvers.editor.escalate')} checked={escalate} onChange={setEscalate} />
          {problem !== null && <p className={controls.fieldError}>{problem}</p>}
          <button type="button" className={controls.button} onClick={add}>
            {t('approvers.editor.add')}
          </button>
        </>
      )}
    </fieldset>
  );
}
