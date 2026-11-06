import { afterEach, describe, expect, it, vi } from 'vitest';
import { dictionaries, formatBytes, formatDate, formatNumber, getLocale, locales, localizedMessage, resolveLocale, setLocale, t } from './i18n';
import kernelSource from '../crates/core/src/kernel.rs?raw';
import diagnosticMessageCatalog from './diagnostic-message-catalog';

afterEach(() => { setLocale('en'); vi.unstubAllGlobals(); });

describe('system locale normalization', () => {
  it.each([
    ['zh', 'zh-CN'], ['zh_CN.UTF-8', 'zh-CN'], ['zh-SG', 'zh-CN'],
    ['zh-TW', 'zh-TW'], ['zh-HK', 'zh-TW'], ['zh-MO', 'zh-TW'],
    ['zh-Hant', 'zh-TW'], ['zh-Hans', 'zh-CN'], ['zh-Hant-CN', 'zh-TW'],
    ['zh-Hans-HK', 'zh-CN'], ['zh-CHS', 'zh-CN'], ['zh-CHT', 'zh-TW'],
    ['en_US.UTF-8', 'en'], ['en-GB', 'en'], [' JA_jp ', 'ja'], ['ko_KR', 'ko'],
    ['de_DE@euro', 'de'], ['fr-CA', 'fr'], ['es-MX', 'es'], ['ru_RU', 'ru'],
    ['pt-PT', 'pt-BR'], ['pt_BR', 'pt-BR'], ['pt', 'pt-BR'],
    ['it-IT', 'en'], ['ar-SA', 'en'], ['C', 'en'], ['POSIX', 'en'],
  ])('%s resolves to %s', (input, expected) => {
    expect(resolveLocale(input)).toBe(expected);
  });

  it('uses the browser language only for auto or empty choices', () => {
    vi.stubGlobal('navigator', { languages: ['ko-KR', 'en-US'], language: 'en-US' });
    expect(resolveLocale('auto')).toBe('ko');
    expect(resolveLocale('')).toBe('ko');
    expect(resolveLocale('de')).toBe('de');
  });

  it('has a deterministic fallback without a browser', () => {
    vi.stubGlobal('navigator', undefined);
    expect(resolveLocale('auto')).toBe('en');
  });
});

describe('bundled translation catalogs', () => {
  const referenceKeys = Object.keys(dictionaries.en).sort();
  const variables = (s: string) => [...s.matchAll(/\{([a-zA-Z0-9_]+)\}/g)].map(m => m[1]).sort();

  it('ships ten complete native-named language packs', () => {
    expect(locales).toHaveLength(10);
    expect(new Set(locales.map(locale => locale.code)).size).toBe(10);
    expect(referenceKeys.length).toBeGreaterThan(30);
  });

  for (const locale of locales) {
    it(`${locale.code} has key and placeholder parity, plain text, and no empty strings`, () => {
      const catalog = dictionaries[locale.code as keyof typeof dictionaries];
      expect(Object.keys(catalog).sort()).toEqual(referenceKeys);
      for (const key of referenceKeys) {
        expect(catalog[key].trim(), `${locale.code}:${key}`).not.toBe('');
        expect(variables(catalog[key]), `${locale.code}:${key}`).toEqual(variables(dictionaries.en[key]));
        expect(catalog[key], `${locale.code}:${key}`).not.toMatch(/<\/?[a-z][^>]*>/i);
      }
    });
  }

  it('keeps required East Asian language navigation translated', () => {
    expect(dictionaries['zh-CN']['nav.kernel']).toBe('内核');
    expect(dictionaries['zh-TW']['nav.kernel']).toBe('核心');
    expect(dictionaries.ja['nav.kernel']).toBe('カーネル');
    expect(dictionaries.ko['nav.kernel']).toBe('커널');
  });

  it('explains session-only settings recovery and names the explicit Save action in every language', () => {
    for (const locale of locales) {
      setLocale(locale.code);
      const message = t('settings.sessionOnly');
      expect(message).not.toBe('settings.sessionOnly');
      expect(message).toContain(t('common.save'));
      if (locale.code !== 'en') expect(message).not.toBe(dictionaries.en['settings.sessionOnly']);
    }
    expect(dictionaries['zh-CN']['settings.sessionOnly']).toBe('设置加载失败：当前语言、主题和路径变更仅用于本次会话。明确点击“保存设置”后才会重建配置文件。');
  });

  it('distinguishes local DLL evidence from the selected source in every language', () => {
    const required = [
      'kernel.installedTitle', 'kernel.installedDescription', 'kernel.installedAbsent',
      'kernel.installedAbsentDescription', 'kernel.installedNoSteam', 'kernel.installedNoSteamDescription',
      'kernel.installedVersion', 'kernel.selectedVersion', 'kernel.installedDirectory',
      'kernel.installedFile', 'kernel.installedHash', 'kernel.loadedUnverified',
      'kernel.selectedSource', 'kernel.sourceVersionNote', 'kernel.installedDetected',
      'common.unsaved', 'settings.pathsSavedRescan',
    ];
    for (const locale of locales) {
      setLocale(locale.code);
      for (const key of required) {
        expect(t(key), `${locale.code}:${key}`).not.toBe(key);
        expect(t(key), `${locale.code}:${key}`).not.toMatch(/\b\d+\.\d+\.\d+(?:\.\d+)?\b/);
      }
      expect(t('kernel.installedVersion')).not.toBe(t('kernel.selectedVersion'));
      expect(t('kernel.installedDirectory')).not.toBe(t('kernel.installedFile'));
      expect(t('kernel.version')).toBe(t('kernel.selectedVersion'));
    }
  });

  it('labels file detection rather than claiming deployment or a loaded kernel', () => {
    expect(dictionaries.en['kernel.installedDetected']).toBe('Primary DLL file detected');
    expect(dictionaries['zh-CN']['kernel.installedDetected']).toBe('检出主 DLL 文件');
    expect(dictionaries.en['kernel.loadedUnverified']).toBe('In-process load state unverified');
    expect(dictionaries['zh-CN']['kernel.sourceVersionNote']).toBe('此版本属于所选内容，不代表本机已安装内核。');
    expect(dictionaries.en['diagnostic.installed_kernel_detected.title']).toBe('Local primary kernel DLL file detected');
    expect(dictionaries['zh-CN']['diagnostic.installed_kernel_detected.title']).toBe('检出本机内核主 DLL 文件');
    expect(dictionaries.en['diagnostic.installed_version_unknown.title']).toBe('Local kernel DLL file version unknown');
    expect(dictionaries['zh-CN']['diagnostic.installed_version_unknown.title']).toBe('本机内核 DLL 文件版本未知');
  });

  it('covers every installation state, injection entry and library view in all languages', () => {
    const required = [
      'kernel.installStatus', 'kernel.injectionMethod', 'kernel.remnants', 'kernel.conflicts', 'kernel.fileCount',
      ...['notInstalled', 'enabled', 'disabled', 'conflict', 'incomplete'].map(status => `kernel.installStatus.${status}`),
      ...['none', 'dwmapi', 'version', 'both'].map(method => `kernel.injectionMethod.${method}`),
      'library.view', 'library.view.cards', 'library.view.list',
      'diagnostic.kernel_installation_not_installed.title', 'diagnostic.kernel_installation_enabled.title',
      'diagnostic.kernel_installation_disabled.title', 'diagnostic.kernel_installation_conflict.title',
      'diagnostic.kernel_installation_incomplete.title', 'diagnostic.kernel_installation_remnants.title',
      'diagnostic.kernel_installation.not_installed.body', 'diagnostic.kernel_installation.enabled.body',
      'diagnostic.kernel_installation.disabled.body', 'diagnostic.kernel_installation.conflict.body',
      'diagnostic.kernel_installation.incomplete.body', 'diagnostic.kernel_installation.remnants.body',
    ];
    for (const locale of locales) {
      const catalog = dictionaries[locale.code as keyof typeof dictionaries];
      for (const key of required) expect(catalog[key]?.trim(), `${locale.code}:${key}`).toBeTruthy();
    }
  });

  it('covers every statically declared Rust kernel diagnostic title in all language packs', () => {
    const production = kernelSource.split('#[cfg(test)]')[0];
    const codes = [...new Set([...production.matchAll(
      /\b(?:diagnostic|message_diagnostic|issue_diagnostic|diagnostic_from_args)\(\s*"([^"]+)"\s*,\s*"(?:info|success|warning|error)"\s*,\s*"[^"]*"/g,
    )].map(match => match[1]))];
    expect(codes.length).toBeGreaterThan(35);
    expect(codes).toContain('installed_kernel_detected');
    expect(codes).toContain('installed_version_unknown');
    const missing: string[] = [];
    for (const locale of locales) {
      const catalog = dictionaries[locale.code as keyof typeof dictionaries];
      for (const code of codes) {
        const key = `diagnostic.${code}.title`;
        if (!Object.prototype.hasOwnProperty.call(catalog, key) || !catalog[key].trim()) {
          missing.push(`${locale.code}:${key}`);
        }
      }
    }
    expect(missing).toEqual([]);
  });

  it('covers every diagnostic body and preserves its complete declared argument set', () => {
    const messages: Record<string, { template: string; args: string[] }> = diagnosticMessageCatalog;
    expect(Object.keys(messages).length).toBeGreaterThanOrEqual(72);
    for (const [key, specification] of Object.entries(messages)) {
      expect(dictionaries.en[key], key).toBe(specification.template);
      const expected = [...specification.args].sort();
      expect([...new Set(variables(specification.template))].sort(), key).toEqual(expected);
      for (const locale of locales) {
        const catalog = dictionaries[locale.code as keyof typeof dictionaries];
        expect(Object.prototype.hasOwnProperty.call(catalog, key), `${locale.code}:${key}`).toBe(true);
        expect([...new Set(variables(catalog[key]))].sort(), `${locale.code}:${key}`).toEqual(expected);
        if (locale.code !== 'en' && expected.length === 0 && specification.template.length > 25) {
          expect(catalog[key], `${locale.code}:${key}`).not.toBe(specification.template);
        }
        setLocale(locale.code);
        const args = Object.fromEntries(specification.args.map(name => [name, `VALUE_${name.toUpperCase()}`]));
        const rendered = localizedMessage(key, args);
        for (const value of Object.values(args)) expect(rendered, `${locale.code}:${key}`).toContain(value);
        if (key !== 'diagnostic.detail.unavailable') expect(rendered, `${locale.code}:${key}`).not.toBe(t('diagnostic.detail.unavailable'));
      }
    }
  });

});

describe('structured localized diagnostic and operation messages', () => {
  it('keeps source versions and paths without treating a declared version as installation evidence', () => {
    setLocale('zh-CN');
    expect(localizedMessage('diagnostic.source_version.body', { filename: 'src/CMakeLists.txt', version: '1.5.4.1' }))
      .toBe('src/CMakeLists.txt 声明 OpenSteamTool 版本为 1.5.4.1。这是源码声明，不是经过验证的发布标签。');
    setLocale('ja');
    expect(localizedMessage('diagnostic.source_version.body', { filename: 'src/CMakeLists.txt', version: '1.5.4.1' }))
      .toContain('ソース上の宣言であり、検証済みのリリースタグではありません。');
  });

  it('formats duplicate-key position and one nested reason without losing dynamic evidence', () => {
    setLocale('zh-CN');
    expect(localizedMessage('diagnostic.config_invalid.duplicate', { line: '62', column: '17' }))
      .toBe('TOML 键或表重复，位于第 62 行、第 17 列；源值已省略。');
    expect(localizedMessage('diagnostic.metadata_invalid.body', {
      channel: 'pattern', component: 'steamclient', path: 'C:\\fixture\\metadata.toml',
      reasonKey: 'diagnostic.reason.toml_duplicate', line: '62', column: '17',
    })).toBe('pattern/steamclient，位置 C:\\fixture\\metadata.toml：TOML 键或表重复，位于第 62 行、第 17 列；源值已省略。');
  });

  it('preserves literal placeholders inside argument values in a single interpolation pass', () => {
    setLocale('en');
    expect(localizedMessage('diagnostic.config_template_placeholders.body', {
      field: 'remote.url_template', required: '{channel}, {component}, {sha256}', missing: '{sha256}',
    })).toBe('remote.url_template requires {channel}, {component}, {sha256}; missing placeholders: {sha256}. The configured URL is omitted.');
  });

  it('never returns unknown message keys or arbitrary English errors as primary text', () => {
    setLocale('zh-CN');
    const fallback = t('diagnostic.detail.unavailable');
    expect(localizedMessage(undefined, undefined)).toBe(fallback);
    expect(localizedMessage('diagnostic.future.body', { detail: 'RAW SECRET ERROR' })).toBe(fallback);
    expect(localizedMessage('RAW SECRET ERROR', undefined)).toBe(fallback);
    expect(localizedMessage('constructor', undefined)).toBe(fallback);
    expect(localizedMessage('diagnostic.source_version.body', { version: '1.5.4.1' })).toBe(fallback);
    const reason = localizedMessage('diagnostic.file_unreadable.body', { filename: 'sample.dll', reasonKey: 'RAW SECRET ERROR' });
    expect(reason).toBe(`sample.dll: ${t('diagnostic.reason.unavailable')}`);
    expect(reason).not.toContain('RAW SECRET ERROR');
  });

  it('allows only known operation-envelope error keys and uses a localized fallback for unknown keys', () => {
    setLocale('zh-CN');
    expect(localizedMessage('error.pathRequired', {}, 'app.errorHelp')).toBe('请先选择或输入路径。');
    expect(localizedMessage('error.future', {}, 'app.errorHelp')).toBe(t('app.errorHelp'));
    expect(localizedMessage('error.unclassified', {})).toBe('发生未分类错误。请展开技术记录查看详情。');
  });
});

describe('translation lookup and international formatting', () => {
  it('switches language immediately', () => {
    setLocale('zh-TW');
    expect(getLocale()).toBe('zh-TW');
    expect(t('common.save')).toBe('儲存設定');
    setLocale('ja');
    expect(t('common.save')).toBe('設定を保存');
  });

  it('interpolates literal values without replacement-string interpretation', () => {
    setLocale('en');
    expect(t('app.exported', { path: 'C:\\$&\\{path}.json' })).toBe('Report saved to C:\\$&\\{path}.json');
    expect(t('app.exported')).toBe('Report saved to {path}');
    expect(t('app.exported', {})).toBe('Report saved to {path}');
  });

  it('returns missing keys rather than throwing, including prototype names', () => {
    expect(t('missing.translation.key')).toBe('missing.translation.key');
    expect(t('constructor')).toBe('constructor');
    expect(t('__proto__')).toBe('__proto__');
  });

  it('formats numbers with the selected language, independently of host locale', () => {
    setLocale('de');
    expect(formatNumber(1234.5)).toBe('1.234,5');
    setLocale('en');
    expect(formatNumber(1234.5)).toBe('1,234.5');
  });

  it('formats IEC sizes and handles invalid inputs', () => {
    setLocale('en');
    expect(formatBytes(0)).toBe('0 B');
    expect(formatBytes(1536)).toBe('1.5 KiB');
    expect(formatBytes(-1)).toBe('Unknown');
    expect(formatBytes(Number.NaN)).toBe('Unknown');
    expect(formatBytes(Number.POSITIVE_INFINITY)).toBe('Unknown');
    expect(formatBytes(0.5)).not.toContain('undefined');
    expect(formatDate(new Date('invalid'))).toBe('Unknown');
  });
});
