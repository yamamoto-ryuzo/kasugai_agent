// 軽量 i18n（kasugai_canvas web/shared/i18n.js のパネル用クラシックスクリプト版）。
// 辞書は ./i18n/<lang>.json、言語は localStorage("kasugaiLanguage") に保存
// （Canvas web/shared/i18n.js と同一キー名）。
// 未設定時はブラウザ言語を初期値にし、未対応なら英語にフォールバック。
// data-i18n / data-i18n-html / data-i18n-attr 属性で静的DOMを一括置換する。
// 辞書は en(フォールバック)と現在言語のみ読み込み、切替時に必要なものを遅延取得する。

const I18N_STORAGE_KEY = "kasugaiLanguage";
const SUPPORTED_LANGUAGES = ["en", "ja", "zh", "ko", "es", "fr", "de", "pt"];
const LANGUAGE_NAMES = {
  en: "English",
  ja: "日本語",
  zh: "中文",
  ko: "한국어",
  es: "Español",
  fr: "Français",
  de: "Deutsch",
  pt: "Português",
};

let i18nLang = "en";
const i18nDicts = {};
const i18nLoading = {};
let i18nInitPromise = null;

async function i18nLoadDict(lang) {
  try {
    const response = await fetch(`i18n/${lang}.json`, { cache: "no-store" });
    if (response.ok) return await response.json();
  } catch (error) {
    console.warn(`[i18n] failed to load dict: ${lang}`, error);
  }
  return {};
}

function i18nEnsureDict(lang) {
  if (!i18nDicts[lang]) {
    if (!i18nLoading[lang]) i18nLoading[lang] = i18nLoadDict(lang).then(d => { i18nDicts[lang] = d; });
    return i18nLoading[lang];
  }
  return Promise.resolve();
}

function i18nNormalize(lang) {
  if (typeof lang !== "string") return "en";
  if (SUPPORTED_LANGUAGES.includes(lang)) return lang;
  const base = lang.toLowerCase().split("-")[0];
  return SUPPORTED_LANGUAGES.includes(base) ? base : "en";
}

function getLanguage() {
  return i18nLang;
}

function initI18n() {
  if (!i18nInitPromise) {
    i18nInitPromise = (async () => {
      let stored = null;
      try { stored = localStorage.getItem(I18N_STORAGE_KEY); } catch (e) {}
      let nav = "";
      try { nav = navigator.language || ""; } catch (e) {}
      i18nLang = i18nNormalize(stored || nav || "en");
      await Promise.all([i18nEnsureDict("en"), i18nEnsureDict(i18nLang)]);
      document.documentElement.lang = i18nLang;
    })();
  }
  return i18nInitPromise;
}

function t(key, params) {
  let text = i18nDicts[i18nLang]?.[key] ?? i18nDicts.en?.[key] ?? key;
  if (params && typeof params === "object") {
    text = text.replace(/\{(\w+)\}/g, (match, name) => (params[name] != null ? String(params[name]) : match));
  }
  return text;
}

function applyI18n(root = document) {
  root.querySelectorAll("[data-i18n]").forEach(el => { el.textContent = t(el.dataset.i18n); });
  root.querySelectorAll("[data-i18n-html]").forEach(el => { el.innerHTML = t(el.dataset.i18nHtml); });
  root.querySelectorAll("[data-i18n-attr]").forEach(el => {
    el.dataset.i18nAttr.split(";").forEach(pair => {
      const index = pair.indexOf(":");
      if (index < 0) return;
      el.setAttribute(pair.slice(0, index).trim(), t(pair.slice(index + 1).trim()));
    });
  });
}

async function setLanguage(lang) {
  lang = i18nNormalize(lang);
  if (lang === i18nLang) return;
  await i18nEnsureDict(lang);
  i18nLang = lang;
  try { localStorage.setItem(I18N_STORAGE_KEY, lang); } catch (e) {}
  document.documentElement.lang = lang;
  applyI18n();
  window.dispatchEvent(new CustomEvent("kasugai:language-changed", { detail: { lang } }));
}
