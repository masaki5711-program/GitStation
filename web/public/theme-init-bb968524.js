// Applies the remembered colour theme before the first paint (a classic,
// blocking, same-origin script in <head>: the CSP forbids an inline one).
// Mirrors src/theme.ts (key and names; tests/theme-init.test.ts checks it).
// The file name carries the first 8 hex digits of its SHA-256, so the
// year-long immutable cache stays correct: change the code, rename the file.
(function () {
  try {
    var theme = window.localStorage.getItem('gitstation.theme');
    if (theme === 'white' || theme === 'navy' || theme === 'dark' || theme === 'system') {
      document.documentElement.setAttribute('data-theme', theme);
    }
  } catch {
    // Storage disabled: the system theme applies (tokens.css).
  }
})();
