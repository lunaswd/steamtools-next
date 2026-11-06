//! Offline human-readable output using the exact same language catalogs as the GUI.
//! Technical `Diagnostic.detail` values remain unchanged in machine-readable reports.
use crate::{settings::resolve_locale, ConfigSummary, Diagnostic};
use std::{collections::BTreeMap, sync::OnceLock};

type Catalog = BTreeMap<String, String>;
type Catalogs = BTreeMap<&'static str, Catalog>;

const SOURCES: [(&str, &str); 10] = [
    ("en", include_str!("../../../src/locales/en.json")),
    ("zh-CN", include_str!("../../../src/locales/zh-CN.json")),
    ("zh-TW", include_str!("../../../src/locales/zh-TW.json")),
    ("ja", include_str!("../../../src/locales/ja.json")),
    ("ko", include_str!("../../../src/locales/ko.json")),
    ("de", include_str!("../../../src/locales/de.json")),
    ("fr", include_str!("../../../src/locales/fr.json")),
    ("es", include_str!("../../../src/locales/es.json")),
    ("pt-BR", include_str!("../../../src/locales/pt-BR.json")),
    ("ru", include_str!("../../../src/locales/ru.json")),
];

fn catalogs() -> &'static Catalogs {
    static CATALOGS: OnceLock<Catalogs> = OnceLock::new();
    CATALOGS.get_or_init(|| {
        SOURCES
            .into_iter()
            .map(|(locale, source)| {
                (
                    locale,
                    serde_json::from_str(source)
                        .expect("bundled language catalog must contain string values"),
                )
            })
            .collect()
    })
}

fn lookup(locale: &str, key: &str) -> Option<&'static str> {
    let all = catalogs();
    all.get(resolve_locale(locale).as_str())
        .and_then(|catalog| catalog.get(key))
        .or_else(|| all.get("en").and_then(|catalog| catalog.get(key)))
        .map(String::as_str)
}

/// Translate a plain-text key, substituting ASCII `{argument_name}` placeholders once.
/// Missing local keys fall back to English; a completely unknown key gets a generic message.
/// Unknown keys are never echoed because they may originate from untrusted diagnostic input.
/// As with the frontend's `t()`, argument text is never recursively interpreted or HTML-escaped.
pub fn translate(locale: &str, key: &str, args: &BTreeMap<String, String>) -> String {
    lookup(locale, key)
        .map(|template| substitute(template, args))
        .unwrap_or_else(|| unclassified_error(locale))
}

fn substitute(template: &str, args: &BTreeMap<String, String>) -> String {
    let mut output = String::with_capacity(template.len());
    let mut offset = 0;
    while offset < template.len() {
        if template.as_bytes()[offset] == b'{' {
            if let Some(relative_end) = template[offset + 1..].find('}') {
                let end = offset + 1 + relative_end;
                let name = &template[offset + 1..end];
                if !name.is_empty()
                    && name
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                {
                    output.push_str(
                        args.get(name)
                            .map(String::as_str)
                            .unwrap_or(&template[offset..=end]),
                    );
                    offset = end + 1;
                    continue;
                }
            }
        }
        let character = template[offset..]
            .chars()
            .next()
            .expect("offset stays on a character boundary");
        output.push(character);
        offset += character.len_utf8();
    }
    output
}

fn unavailable_detail(locale: &str) -> String {
    // The secondary fallback keeps older/mixed build resources readable without exposing a key.
    lookup(locale, "diagnostic.detail.unavailable")
        .or_else(|| lookup(locale, "diagnostics.findings"))
        .unwrap_or("Diagnostic details")
        .to_owned()
}

fn unavailable_reason(locale: &str) -> String {
    lookup(locale, "diagnostic.reason.unavailable")
        .map(str::to_owned)
        .unwrap_or_else(|| unavailable_detail(locale))
}

fn unclassified_error(locale: &str) -> String {
    lookup(locale, "error.unclassified")
        .map(str::to_owned)
        .unwrap_or_else(|| unavailable_detail(locale))
}

fn known_catalog_key(key: &str) -> bool {
    catalogs()
        .get("en")
        .is_some_and(|catalog| catalog.contains_key(key))
}

fn known_diagnostic_key(key: &str) -> bool {
    key.starts_with("diagnostic.") && known_catalog_key(key)
}

/// Render only catalog-approved primary text, never raw technical details or unrecognized keys.
/// A whitelisted reason key is rendered once before substitution into a body's `{reason}` field.
pub fn localized_diagnostic_detail(locale: &str, diagnostic: &Diagnostic) -> String {
    if !known_diagnostic_key(&diagnostic.message_key) {
        return unavailable_detail(locale);
    }
    format_with_reason(locale, &diagnostic.message_key, &diagnostic.message_args)
}

/// Localize a configuration error while retaining raw `config.error` for technical evidence.
/// Legacy reports with no translation metadata receive a generic message, not raw English.
pub fn localized_config_error(locale: &str, config: &ConfigSummary) -> Option<String> {
    if config.valid && config.error.is_none() && config.error_key.is_empty() {
        return None;
    }
    if !known_catalog_key(&config.error_key) {
        return Some(unclassified_error(locale));
    }
    Some(format_with_reason(
        locale,
        &config.error_key,
        &config.error_args,
    ))
}

fn format_with_reason(
    locale: &str,
    message_key: &str,
    message_args: &BTreeMap<String, String>,
) -> String {
    let mut args = message_args.clone();
    if let Some(reason_key) = args.remove("reasonKey") {
        let reason =
            if reason_key.starts_with("diagnostic.reason.") && known_diagnostic_key(&reason_key) {
                translate(locale, &reason_key, &args)
            } else {
                unavailable_reason(locale)
            };
        args.insert("reason".into(), reason);
    }
    translate(locale, message_key, &args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn replaces_arguments_once_and_keeps_missing_placeholders() {
        let args = BTreeMap::from([
            ("count".into(), "42".into()),
            ("path".into(), "日本語/{count}/<plain>".into()),
        ]);
        assert_eq!(
            translate("en", "library.resultCount", &args),
            "Applications: 42"
        );
        assert_eq!(
            translate("en", "app.exported", &args),
            "Report saved to 日本語/{count}/<plain>"
        );
        assert_eq!(
            substitute("{missing} {count} {bad-name}", &args),
            "{missing} 42 {bad-name}"
        );
        assert_eq!(substitute("{a{count}}", &args), "{a42}");
    }

    #[test]
    fn resolves_unknown_locale_and_key_fallbacks() {
        let args = BTreeMap::new();
        assert_eq!(
            translate("unknown-locale", "common.unknown", &args),
            "Unknown"
        );
        assert_eq!(
            translate("ja", "missing.example.key", &args),
            unclassified_error("ja")
        );
        assert!(!translate("ko", "PRIVATE_SECRET_KEY", &args).contains("PRIVATE_SECRET_KEY"));
        assert_eq!(translate("zh-CHT", "common.unknown", &args), "未知");
    }

    #[test]
    fn chinese_japanese_and_korean_share_gui_catalogs() {
        let args = BTreeMap::from([("count".into(), "7".into())]);
        assert_eq!(
            translate("zh-CN", "library.resultCount", &args),
            "应用数量：7"
        );
        assert_eq!(
            translate("ja-JP", "library.resultCount", &args),
            "アプリ数：7"
        );
        assert_eq!(translate("ko-KR", "library.resultCount", &args), "앱 수: 7");
    }

    #[test]
    fn every_catalog_has_all_english_keys_and_placeholder_parity() {
        let all = catalogs();
        let english = all.get("en").unwrap();
        let expected: BTreeSet<_> = english.keys().collect();
        for (locale, catalog) in all {
            assert_eq!(
                catalog.keys().collect::<BTreeSet<_>>(),
                expected,
                "{locale}: key parity"
            );
            for (key, message) in catalog {
                assert!(!message.trim().is_empty(), "{locale}: empty {key}");
                assert_eq!(
                    placeholders(message),
                    placeholders(&english[key]),
                    "{locale}: {key} placeholder parity"
                );
            }
        }
    }

    fn placeholders(template: &str) -> BTreeSet<&str> {
        let mut result = BTreeSet::new();
        for (offset, character) in template.char_indices() {
            if character != '{' {
                continue;
            }
            if let Some(relative_end) = template[offset + 1..].find('}') {
                let name = &template[offset + 1..offset + 1 + relative_end];
                if !name.is_empty()
                    && name
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                {
                    result.insert(name);
                }
            }
        }
        result
    }

    fn diagnostic(message_key: &str, message_args: BTreeMap<String, String>) -> Diagnostic {
        Diagnostic {
            code: "fixture".into(),
            severity: "warning".into(),
            title: "Technical evidence".into(),
            detail: "RAW_PRIVATE_MARKER must not be primary output".into(),
            message_key: message_key.into(),
            message_args,
        }
    }

    #[test]
    fn unknown_or_sensitive_message_keys_never_become_primary_output() {
        for locale in ["zh-CN", "ja", "ko", "en"] {
            for key in [
                "",
                "diagnostic.PRIVATE_TOKEN_MARKER",
                "app.exported",
                "token=PRIVATE_TOKEN_MARKER",
            ] {
                let item = diagnostic(
                    key,
                    BTreeMap::from([("path".into(), "PRIVATE_TOKEN_MARKER".into())]),
                );
                let output = localized_diagnostic_detail(locale, &item);
                assert!(!output.contains("PRIVATE_TOKEN_MARKER"));
                assert!(!output.contains("RAW_PRIVATE_MARKER"));
                // English prose can legitimately end a sentence with "diagnostic.".
                // Verify the exact catalog fallback and the actual untrusted key instead.
                assert_eq!(
                    output,
                    lookup(locale, "diagnostic.detail.unavailable")
                        .expect("bundled localized fallback must exist"),
                    "{locale}: unexpected primary output"
                );
                if !key.is_empty() {
                    assert!(!output.contains(key), "{locale}: untrusted key was echoed");
                }
                assert!(!output.contains("diagnostic.detail.unavailable"));
                assert_eq!(item.detail, "RAW_PRIVATE_MARKER must not be primary output");
            }
        }
    }

    #[test]
    fn known_message_uses_catalog_and_preserves_technical_evidence() {
        let item = diagnostic("diagnostic.runtime_unverified.title", BTreeMap::new());
        for locale in ["zh-CN", "ja", "ko"] {
            assert_eq!(
                localized_diagnostic_detail(locale, &item),
                translate(
                    locale,
                    "diagnostic.runtime_unverified.title",
                    &BTreeMap::new()
                )
            );
        }
        assert_eq!(item.detail, "RAW_PRIVATE_MARKER must not be primary output");
    }

    #[test]
    fn legacy_and_unknown_config_errors_get_localized_generic_text() {
        let mut config = ConfigSummary {
            path: "fixture.toml".into(),
            valid: false,
            sections: vec![],
            unknown_sections: vec![],
            error: Some("RAW_PRIVATE_CONFIG_ERROR".into()),
            error_key: String::new(),
            error_args: BTreeMap::new(),
        };
        for locale in ["zh-CN", "ja", "ko", "en"] {
            let output = localized_config_error(locale, &config).unwrap();
            assert_eq!(output, unclassified_error(locale));
            assert!(!output.contains("RAW_PRIVATE_CONFIG_ERROR"));
        }
        config.error_key = "PRIVATE_SECRET_KEY".into();
        assert!(!localized_config_error("ja", &config)
            .unwrap()
            .contains("PRIVATE_SECRET_KEY"));
        assert_eq!(config.error.as_deref(), Some("RAW_PRIVATE_CONFIG_ERROR"));
        config.valid = true;
        config.error = None;
        config.error_key.clear();
        assert_eq!(localized_config_error("ja", &config), None);
    }

    #[test]
    fn diagnostic_and_config_render_one_level_of_localized_reason() {
        let args = BTreeMap::from([
            ("filename".into(), "fixture.toml".into()),
            ("reasonKey".into(), "diagnostic.reason.io_not_found".into()),
            ("osCode".into(), "2".into()),
        ]);
        let item = diagnostic("diagnostic.config_unreadable.body", args.clone());
        let config = ConfigSummary {
            path: "fixture.toml".into(),
            valid: false,
            sections: vec![],
            unknown_sections: vec![],
            error: Some("RAW_CONFIG_EVIDENCE".into()),
            error_key: item.message_key.clone(),
            error_args: args,
        };
        for locale in ["zh-CN", "ja", "ko", "en"] {
            assert!(lookup(locale, "diagnostic.config_unreadable.body").is_some());
            assert!(lookup(locale, "diagnostic.reason.io_not_found").is_some());
            let reason = translate(locale, "diagnostic.reason.io_not_found", &item.message_args);
            let output = localized_diagnostic_detail(locale, &item);
            assert!(output.contains("fixture.toml"));
            assert!(output.contains(&reason));
            assert!(!output.contains("reasonKey"));
            assert!(!output.contains("{reason}"));
            assert_eq!(
                localized_config_error(locale, &config).as_deref(),
                Some(output.as_str())
            );
            assert_eq!(config.error.as_deref(), Some("RAW_CONFIG_EVIDENCE"));
        }
    }

    #[test]
    fn unknown_reason_key_is_generic_and_never_echoed() {
        let item = diagnostic(
            "diagnostic.file_unreadable.body",
            BTreeMap::from([
                ("filename".into(), "fixture.dll".into()),
                (
                    "reasonKey".into(),
                    "diagnostic.reason.PRIVATE_SECRET_KEY".into(),
                ),
            ]),
        );
        for locale in ["zh-CN", "ja", "ko", "en"] {
            assert!(lookup(locale, "diagnostic.file_unreadable.body").is_some());
            let output = localized_diagnostic_detail(locale, &item);
            assert!(output.contains(&unavailable_reason(locale)));
            assert!(!output.contains("PRIVATE_SECRET_KEY"));
            assert!(!output.contains("RAW_PRIVATE_MARKER"));
        }
    }
}
