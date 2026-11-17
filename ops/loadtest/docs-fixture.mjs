// synthetic — the document repository fixture (plan 13 format): one doctype
// and N procedure documents, each with sections and `::: step` blocks.
export const DOCTYPE = String.raw`# synthetic — load-test doctype (plan 13 format)
name: lt.procedure
version: 1
label: { ja: 試験の手順書, en: Load-test procedure, id: Prosedur uji }
id_pattern: '^LT-\d{4}$'
fields:
  scope_sites: { type: list, required: false, label: { ja: 適用する事業所 } }
sections:
  - { title: 目的 }
  - { title: 手順, blocks: [step] }
  - { title: 記録 }
blocks:
  step:
    id_pattern: '^S-\d{1,2}$'
    attrs:
      role: { type: text, required: true }
`;

export const docId = (n) => `LT-${String(n).padStart(4, '0')}`;
export const docPath = (n) => `docs/procedures/${docId(n)}.md`;

/** A document; `edits` maps a step number to replacement text (a revision). */
export function doc(n, { purpose = null, edits = {}, steps = 10 } = {}) {
  const id = docId(n);
  const lines = [
    '---', 'doctype: lt.procedure', `id: ${id}`, `title: 合成の手順書 ${n}`, 'owner: qa-team',
    'classification: internal', 'scope_sites: [第一工場]', '---', '',
    '<!-- synthetic: 負荷試験の合成の文書。実在の手順ではない。 -->', '',
    '## 目的', '', purpose ?? `この手順書 ${id} は、合成の設備 ${n} の点検の手順を定める。`, '',
    '## 手順', '',
  ];
  for (let s = 1; s <= steps; s++) {
    lines.push(`::: step S-${s} role=作業者`);
    lines.push(edits[s] ?? `手順 ${s}: 設備 ${n} の部位 ${s} を目で見て確かめる。`);
    lines.push(':::', '');
  }
  lines.push('## 記録', '', '点検の結果は点検表に残す。', '');
  return lines.join('\n');
}
