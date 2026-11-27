(() => {
  let pref = null;
  try {
    const raw = localStorage.getItem('starfane_theme');
    if (raw === 'dark' || raw === 'light') pref = raw;
  } catch (err) {
    console.debug('theme-init storage read skipped:', err);
  }
  const sysDark = typeof window.matchMedia === 'function'
    && window.matchMedia('(prefers-color-scheme: dark)').matches;
  const theme = pref || (sysDark ? 'dark' : 'light');
  const root = document.documentElement;
  root.dataset.theme = theme;
  root.dataset.themePref = pref || 'auto';
  const meta = document.querySelector('meta[name="theme-color"]');
  if (meta) meta.setAttribute('content', theme === 'dark' ? '#141413' : '#FAF9F6');
})();
