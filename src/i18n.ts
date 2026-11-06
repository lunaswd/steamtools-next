import en from './locales/en.json';
import zhCN from './locales/zh-CN.json';
import zhTW from './locales/zh-TW.json';
import ja from './locales/ja.json';
import ko from './locales/ko.json';
import de from './locales/de.json';
import fr from './locales/fr.json';
import es from './locales/es.json';
import ptBR from './locales/pt-BR.json';
import ru from './locales/ru.json';

export type SupportedLocale = 'zh-CN' | 'zh-TW' | 'en' | 'ja' | 'ko' | 'de' | 'fr' | 'es' | 'pt-BR' | 'ru';
type Catalog = Readonly<Record<string, string>>;

/** Bundled, offline language packs. Display names intentionally stay in their native language. */
export const locales: { code: string; name: string }[] = [
  { code: 'zh-CN', name: '简体中文' },
  { code: 'zh-TW', name: '繁體中文' },
  { code: 'en', name: 'English' },
  { code: 'ja', name: '日本語' },
  { code: 'ko', name: '한국어' },
  { code: 'de', name: 'Deutsch' },
  { code: 'fr', name: 'Français' },
  { code: 'es', name: 'Español' },
  { code: 'pt-BR', name: 'Português (Brasil)' },
  { code: 'ru', name: 'Русский' },
];

export const dictionaries: Readonly<Record<SupportedLocale, Catalog>> = {
  'zh-CN': zhCN, 'zh-TW': zhTW, en, ja, ko, de, fr, es, 'pt-BR': ptBR, ru,
};

function browserLocale(): string {
  if (typeof navigator === 'undefined') return 'en';
  return navigator.languages?.[0] || navigator.language || 'en';
}

/** Normalize BCP 47, Windows/POSIX locale names and Chinese script/region variants.
 * Explicit script wins over region: zh-Hans-HK is Simplified, zh-Hant-CN Traditional.
 * Unsupported languages use English; we never infer language from the timezone.
 */
export function resolveLocale(input: string): SupportedLocale {
  const raw = input.trim();
  if (!raw || raw.toLowerCase() === 'auto') return normalizeLocale(browserLocale());
  return normalizeLocale(raw);
}

function normalizeLocale(input: string): SupportedLocale {
  const tag = input.trim().toLowerCase().replace(/_/g, '-').split(/[.@]/, 1)[0];
  const parts = tag.split('-').filter(Boolean);
  const language = parts[0];
  if (language === 'zh') {
    if (parts.includes('hans') || parts.includes('chs')) return 'zh-CN';
    if (parts.includes('hant') || parts.includes('cht')) return 'zh-TW';
    if (parts.some(part => ['tw', 'hk', 'mo'].includes(part))) return 'zh-TW';
    return 'zh-CN';
  }
  if (language === 'pt') return 'pt-BR';
  if (['en', 'ja', 'ko', 'de', 'fr', 'es', 'ru'].includes(language)) return language as SupportedLocale;
  return 'en';
}

let activeLocale: SupportedLocale = 'en';

export function setLocale(code: string): void {
  activeLocale = resolveLocale(code);
  if (typeof document !== 'undefined') {
    document.documentElement.lang = activeLocale;
    document.documentElement.dir = 'ltr';
  }
}

export function getLocale(): SupportedLocale {
  return activeLocale;
}

/** Returns plain text only. Callers must escape it if using HTML strings. */
export function t(key: string, vars?: Record<string, string | number>): string {
  const localized = dictionaries[activeLocale][key];
  const fallback = dictionaries.en[key];
  const message = typeof localized === 'string' ? localized : typeof fallback === 'string' ? fallback : key;
  if (!vars) return message;
  return message.replace(/\{([a-zA-Z0-9_]+)\}/g, (placeholder, name: string) =>
    Object.prototype.hasOwnProperty.call(vars, name) ? String(vars[name]) : placeholder,
  );
}

/** Render structured diagnostic/configuration messages without parsing English.
 * Unknown keys and incomplete argument sets show a localized fallback. Raw evidence
 * belongs in a separate disclosure, never in this primary message.
 */
export function localizedMessage(
  key: string | undefined,
  args: Record<string, string> | undefined,
  fallbackKey = 'diagnostic.detail.unavailable',
): string {
  const catalog = dictionaries[activeLocale];
  const known = (candidate: string) => Object.prototype.hasOwnProperty.call(catalog, candidate)
    && typeof catalog[candidate] === 'string';
  const fallback = known(fallbackKey) ? t(fallbackKey) : t('diagnostic.detail.unavailable');
  if (!key || !(key.startsWith('diagnostic.') || key.startsWith('error.')) || !known(key)) return fallback;
  const values: Record<string, string> = { ...(args ?? {}) };
  if (Object.prototype.hasOwnProperty.call(values, 'reasonKey')) {
    const reasonKey = values.reasonKey;
    const reasonTemplate = reasonKey.startsWith('diagnostic.') && known(reasonKey) ? catalog[reasonKey] : null;
    const complete = reasonTemplate !== null && [...reasonTemplate.matchAll(/\{([a-zA-Z0-9_]+)\}/g)]
      .every(match => Object.prototype.hasOwnProperty.call(values, match[1]));
    values.reason = complete ? t(reasonKey, values) : t('diagnostic.reason.unavailable');
    delete values.reasonKey;
  }
  if ([...catalog[key].matchAll(/\{([a-zA-Z0-9_]+)\}/g)]
    .some(match => !Object.prototype.hasOwnProperty.call(values, match[1]))) return fallback;
  return t(key, values);
}

export function formatNumber(value: number, options?: Intl.NumberFormatOptions): string {
  return new Intl.NumberFormat(activeLocale, options).format(value);
}

export function formatDate(value: number | Date, options?: Intl.DateTimeFormatOptions): string {
  const date = value instanceof Date ? value : new Date(value);
  if (!Number.isFinite(date.getTime())) return t('common.unknown');
  return new Intl.DateTimeFormat(activeLocale, options ?? { dateStyle: 'medium', timeStyle: 'short' }).format(date);
}

/** File sizes use IEC units, and the decimal separator follows the UI locale. */
export function formatBytes(value: number): string {
  if (!Number.isFinite(value) || value < 0) return t('common.unknown');
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB', 'PiB'];
  const index = value === 0 ? 0 : Math.max(0, Math.min(Math.floor(Math.log(value) / Math.log(1024)), units.length - 1));
  return `${formatNumber(value / 1024 ** index, { maximumFractionDigits: index === 0 ? 0 : 1 })} ${units[index]}`;
}
