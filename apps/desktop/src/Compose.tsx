/** Collects the one or two fields an action needs before it can be planned. */
import { useEffect, useRef, useState } from "react";

export type Field =
  | { kind: "text"; name: string; label: string; placeholder?: string; hint?: string }
  | { kind: "check"; name: string; label: string; hint?: string };

export type Values = Record<string, string | boolean>;

export function Compose({
  title,
  fields,
  submit,
  onSubmit,
  onCancel,
}: {
  title: string;
  fields: Field[];
  /** The control's label, stated as its effect rather than as "OK". */
  submit: string;
  onSubmit: (values: Values) => void;
  onCancel: () => void;
}) {
  const [values, setValues] = useState<Values>(() =>
    Object.fromEntries(fields.map((f) => [f.name, f.kind === "check" ? false : ""])),
  );
  const first = useRef<HTMLInputElement>(null);
  useEffect(() => first.current?.focus(), []);

  const ready = fields.every(
    (f) => f.kind !== "text" || String(values[f.name] ?? "").trim() !== "",
  );

  return (
    <form
      className="compose"
      onSubmit={(e) => {
        e.preventDefault();
        if (ready) onSubmit(values);
      }}
      onKeyDown={(e) => {
        if (e.key === "Escape") {
          e.stopPropagation();
          onCancel();
        }
      }}
    >
      <h3 className="compose__title">{title}</h3>
      {fields.map((f, i) =>
        f.kind === "text" ? (
          <label key={f.name} className="compose__field">
            <span>{f.label}</span>
            <input
              ref={i === 0 ? first : undefined}
              value={String(values[f.name] ?? "")}
              placeholder={f.placeholder}
              spellCheck={false}
              autoComplete="off"
              onChange={(e) => setValues((v) => ({ ...v, [f.name]: e.target.value }))}
            />
            {f.hint && <small>{f.hint}</small>}
          </label>
        ) : (
          <label key={f.name} className="compose__check">
            <input
              type="checkbox"
              checked={Boolean(values[f.name])}
              onChange={(e) => setValues((v) => ({ ...v, [f.name]: e.target.checked }))}
            />
            <span>
              {f.label}
              {f.hint && <small>{f.hint}</small>}
            </span>
          </label>
        ),
      )}
      <div className="compose__actions">
        <button type="button" onClick={onCancel}>
          Cancel
        </button>
        <button type="submit" className="primary" disabled={!ready}>
          {submit}
        </button>
      </div>
    </form>
  );
}
