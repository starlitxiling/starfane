// starfane web console — deployment-time i18n helper (zh default / en via [web] lang or --lang)
let currentLang = String(document.documentElement?.dataset?.lang || 'zh')
  .toLowerCase()
  .startsWith('en')
  ? 'en'
  : 'zh';

export function getLang() {
  return currentLang;
}

export function isEn() {
  return currentLang === 'en';
}

export function localeTag() {
  return currentLang === 'en' ? 'en-US' : 'zh-CN';
}

export function L(zh, en) {
  return currentLang === 'en' ? en : zh;
}

export function setLang(raw) {
  const next = String(raw || '').toLowerCase().startsWith('en') ? 'en' : 'zh';
  if (next === currentLang) return false;
  currentLang = next;
  if (document.documentElement) {
    document.documentElement.lang = next === 'en' ? 'en' : 'zh-CN';
    document.documentElement.dataset.lang = next;
  }
  return true;
}
