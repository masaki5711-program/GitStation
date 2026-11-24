// The ja dictionary split by the Vite plugin in vite.config.ts
// (scripts/i18n-split.mjs): the strings the first load uses, and the rest.
declare module 'virtual:i18n/ja-core' {
  const strings: Record<string, unknown>;
  export default strings;
}
declare module 'virtual:i18n/ja-rest' {
  const strings: Record<string, unknown>;
  export default strings;
}
