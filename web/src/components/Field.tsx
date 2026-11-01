// Form fields with their label, help and error wired for assistive tech.
import { useId, type ReactNode } from 'react';

import styles from './controls.module.css';

export function TextField({
  label,
  value,
  onChange,
  error,
  help,
  required,
  multiline,
  autoFocus,
}: {
  label: string;
  value: string;
  onChange: (value: string) => void;
  error?: string | null;
  help?: string;
  required?: boolean;
  multiline?: boolean;
  autoFocus?: boolean;
}) {
  const id = useId();
  const helpId = `${id}-help`;
  const errorId = `${id}-error`;
  const describedBy =
    [help === undefined ? null : helpId, error ? errorId : null].filter(Boolean).join(' ') ||
    undefined;
  const common = {
    id,
    className: styles.input,
    value,
    required: required === true,
    'aria-invalid': error ? true : undefined,
    'aria-describedby': describedBy,
    autoFocus,
  };
  return (
    <div className={styles.field}>
      <label htmlFor={id} className={styles.label}>
        {label}
      </label>
      {multiline === true ? (
        <textarea
          {...common}
          rows={3}
          onChange={(e) => {
            onChange(e.target.value);
          }}
        />
      ) : (
        <input
          {...common}
          type="text"
          onChange={(e) => {
            onChange(e.target.value);
          }}
        />
      )}
      {help !== undefined && (
        <p id={helpId} className={styles.help}>
          {help}
        </p>
      )}
      {error ? (
        <p id={errorId} className={styles.fieldError}>
          {error}
        </p>
      ) : null}
    </div>
  );
}

export function SelectField({
  label,
  value,
  onChange,
  error,
  children,
}: {
  label: string;
  value: string;
  onChange: (value: string) => void;
  error?: string | null;
  children: ReactNode;
}) {
  const id = useId();
  const errorId = `${id}-error`;
  return (
    <div className={styles.field}>
      <label htmlFor={id} className={styles.label}>
        {label}
      </label>
      <select
        id={id}
        className={styles.input}
        value={value}
        aria-invalid={error ? true : undefined}
        aria-describedby={error ? errorId : undefined}
        onChange={(e) => {
          onChange(e.target.value);
        }}
      >
        {children}
      </select>
      {error ? (
        <p id={errorId} className={styles.fieldError}>
          {error}
        </p>
      ) : null}
    </div>
  );
}

export interface Choice<T extends string> {
  value: T;
  label: string;
  help: string;
}

export function RadioField<T extends string>({
  legend,
  name,
  value,
  onChange,
  choices,
}: {
  legend: string;
  name: string;
  value: T;
  onChange: (value: T) => void;
  choices: Choice<T>[];
}) {
  const id = useId();
  return (
    <fieldset className={styles.field}>
      <legend className={styles.label}>{legend}</legend>
      {choices.map((c) => (
        <div key={c.value} className={styles.choice}>
          <input
            id={`${id}-${c.value}`}
            type="radio"
            name={name}
            value={c.value}
            checked={value === c.value}
            aria-describedby={`${id}-${c.value}-help`}
            onChange={() => {
              onChange(c.value);
            }}
          />
          <div>
            <label htmlFor={`${id}-${c.value}`}>{c.label}</label>
            <p id={`${id}-${c.value}-help`} className={styles.help}>
              {c.help}
            </p>
          </div>
        </div>
      ))}
    </fieldset>
  );
}
