# src/ui — the only place that may import Radix

ADR-0022 §3a: some Radix components inject a `<style>` element at run time,
which the shell CSP (`style-src 'self'`) blocks. Only the wrappers in this
directory may import Radix (eslint `no-restricted-imports`), so that

- menus, popovers and selects are always `modal={false}`,
- modals are the HTML `<dialog>` element (`Modal.tsx`), never Radix `Dialog`,
- `ScrollArea` is never used.

Allow-list of Radix components (add a row only after the component's page
shows no CSP violation in the Playwright check and no new `<style>` in the
vitest check):

| Component | Wrapper | Notes |
|---|---|---|
| (none yet — M2 MR2 ships no Radix) | | |
