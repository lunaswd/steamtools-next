//! Shared report envelope and user-home redaction for desktop exports.
use crate::{KernelComparison, KernelInstallation, KernelReport, SteamSnapshot};
use serde_json::{json, Value};
use std::{
    env,
    time::{SystemTime, UNIX_EPOCH},
};

/// Build a deterministic-schema, privacy-filtered workspace report.
/// No runtime compatibility is inferred from on-disk state alone.
pub fn build_report(steam: Option<SteamSnapshot>, kernel: Option<KernelReport>) -> Value {
    build_report_with_context(steam, kernel, None, None)
}

/// Include source comparison and installed-kernel evidence without asserting runtime validation.
/// All sections are privacy-filtered together so nested paths receive the same protection.
pub fn build_report_with_context(
    steam: Option<SteamSnapshot>,
    kernel: Option<KernelReport>,
    comparison: Option<KernelComparison>,
    installed_kernel: Option<KernelReport>,
) -> Value {
    build_report_with_installation_context(steam, kernel, comparison, installed_kernel, None)
}

/// Include the richer on-disk installation state while preserving the schema-1
/// fields consumed by older GUI releases. `installedKernel` remains the
/// legacy report; `kernelInstallation` is an independent nullable addition.
pub fn build_report_with_installation_context(
    steam: Option<SteamSnapshot>,
    kernel: Option<KernelReport>,
    comparison: Option<KernelComparison>,
    installed_kernel: Option<KernelReport>,
    kernel_installation: Option<KernelInstallation>,
) -> Value {
    let homes: Vec<String> = ["USERPROFILE", "HOME"]
        .iter()
        .filter_map(|name| env::var_os(name).map(|value| value.to_string_lossy().into_owned()))
        .collect();
    let mut report = json!({
        "schemaVersion": 1,
        "tool": "SteamTools Next",
        "toolVersion": env!("CARGO_PKG_VERSION"),
        "generatedAt": SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs(),
        "inspectionMode": "management-and-inspection",
        "runtimeCompatibilityVerified": false,
        "steam": steam,
        "kernel": kernel,
        "comparison": comparison,
        "installedKernel": installed_kernel,
        "kernelInstallation": kernel_installation,
    });
    redact_with_homes(&mut report, &homes);
    report
}

fn redact_with_homes(value: &mut Value, homes: &[String]) {
    match value {
        Value::String(text) => {
            for home in homes {
                let home = home.trim_end_matches(['/', '\\']);
                // Do not replace a malformed empty home, filesystem root or drive prefix.
                if home.chars().count() >= 3 {
                    *text = redact_home(text, home);
                }
            }
        }
        Value::Array(items) => items
            .iter_mut()
            .for_each(|item| redact_with_homes(item, homes)),
        Value::Object(items) => items
            .values_mut()
            .for_each(|item| redact_with_homes(item, homes)),
        _ => {}
    }
}

fn same_path_char(left: char, right: char) -> bool {
    if matches!(left, '/' | '\\') && matches!(right, '/' | '\\') {
        return true;
    }
    left.to_lowercase().eq(right.to_lowercase())
}

fn path_boundary(next: Option<char>) -> bool {
    match next {
        None => true,
        Some(ch) => {
            matches!(
                ch,
                '/' | '\\' | '\'' | '"' | ':' | ')' | ']' | '}' | ',' | ';'
            ) || ch.is_whitespace()
        }
    }
}

fn redact_home(text: &str, home: &str) -> String {
    let pattern: Vec<char> = home.chars().collect();
    let mut output = String::with_capacity(text.len());
    let mut offset = 0;
    while offset < text.len() {
        let mut cursor = offset;
        let mut matched = true;
        for expected in &pattern {
            let Some(actual) = text[cursor..].chars().next() else {
                matched = false;
                break;
            };
            if !same_path_char(actual, *expected) {
                matched = false;
                break;
            }
            cursor += actual.len_utf8();
        }
        if matched && path_boundary(text[cursor..].chars().next()) {
            output.push_str("%USERPROFILE%");
            offset = cursor;
        } else {
            let next = text[offset..]
                .chars()
                .next()
                .expect("offset is a valid character boundary");
            output.push(next);
            offset += next.len_utf8();
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_nested_case_variants_and_slashes() {
        let mut report = json!({"path":"C:\\Users\\Alice\\Steam", "nested":[{"detail":"Read c:/USERS/aLiCe/Games failed"}], "other":42});
        redact_with_homes(&mut report, &["C:\\Users\\Alice".into()]);
        assert_eq!(report["path"], "%USERPROFILE%\\Steam");
        assert_eq!(
            report["nested"][0]["detail"],
            "Read %USERPROFILE%/Games failed"
        );
        assert_eq!(report["other"], 42);
    }

    #[test]
    fn handles_unicode_without_byte_offset_corruption() {
        let mut report =
            json!({"path":"前缀 c:/users/玩家/日本語", "accent":"c:/users/äLICE/file"});
        redact_with_homes(
            &mut report,
            &["C:\\Users\\玩家".into(), "C:\\Users\\Älice".into()],
        );
        assert_eq!(report["path"], "前缀 %USERPROFILE%/日本語");
        assert_eq!(report["accent"], "%USERPROFILE%/file");
    }

    #[test]
    fn respects_path_boundaries_and_multiple_occurrences() {
        assert_eq!(
            redact_home("C:\\Users\\AliceElse\\x", "C:\\Users\\Alice"),
            "C:\\Users\\AliceElse\\x"
        );
        assert_eq!(
            redact_home(
                "C:\\Users\\Alice; C:\\Users\\Alice\\game",
                "C:\\Users\\Alice"
            ),
            "%USERPROFILE%; %USERPROFILE%\\game"
        );
        assert_eq!(redact_home("/home/me/.x", "/home/me"), "%USERPROFILE%/.x");
    }

    #[test]
    fn empty_roots_do_not_destroy_reports() {
        let mut report = json!({"path":"C:\\Steam\\file"});
        redact_with_homes(&mut report, &[String::new(), "/".into(), "C:\\".into()]);
        assert_eq!(report["path"], "C:\\Steam\\file");
    }

    #[test]
    fn envelope_always_declares_unverified_runtime_and_optional_sections() {
        let report = build_report(None, None);
        assert_eq!(report["schemaVersion"], 1);
        assert_eq!(report["inspectionMode"], "management-and-inspection");
        assert_eq!(report["runtimeCompatibilityVerified"], false);
        assert_eq!(report["steam"], Value::Null);
        assert_eq!(report["kernel"], Value::Null);
        assert_eq!(report["comparison"], Value::Null);
        assert_eq!(report["installedKernel"], Value::Null);
        assert_eq!(report["kernelInstallation"], Value::Null);
        assert!(report["generatedAt"].as_u64().unwrap() > 0);
    }

    fn kernel_fixture(path: &str, kind: &str) -> KernelReport {
        KernelReport {
            path: path.into(),
            kind: kind.into(),
            version: Some("1.2.3".into()),
            files: vec![],
            config: None,
            diagnostics: vec![],
            lua_files: vec![],
        }
    }

    fn installation_fixture(path: &str) -> KernelInstallation {
        KernelInstallation {
            path: path.into(),
            status: "enabled".into(),
            injection_method: "dwmapi".into(),
            version: Some("1.5.4.1".into()),
            version_source: "peResource".into(),
            installed_sha256: Some("abc123".into()),
            artifact_version: Some("1.5.4.1".into()),
            artifact_sha256: Some("abc123".into()),
            hash_match: Some(true),
            runtime_load_verified: false,
            files: vec![crate::KernelFile {
                name: "OpenSteamTool.dll".into(),
                size_bytes: 123,
                sha256: "abc123".into(),
            }],
            remnants: vec!["bin/dwmapi.dll".into()],
            message_key: "diagnostic.kernel_installation.enabled.body".into(),
            message_args: std::collections::BTreeMap::from([
                ("path".into(), path.into()),
                ("injectionMethod".into(), "dwmapi".into()),
            ]),
            diagnostics: vec![crate::Diagnostic {
                code: "kernel_installation_enabled".into(),
                severity: "success".into(),
                title: "OpenSteamTool is enabled".into(),
                detail: format!("{path}: enabled through dwmapi"),
                message_key: "diagnostic.kernel_installation.enabled.body".into(),
                message_args: std::collections::BTreeMap::from([
                    ("path".into(), path.into()),
                    ("injectionMethod".into(), "dwmapi".into()),
                ]),
            }],
        }
    }

    #[test]
    fn context_preserves_comparison_results_and_installed_evidence() {
        let left = kernel_fixture("E:/fixtures/original", "source");
        let right = kernel_fixture("E:/fixtures/fork", "source");
        let comparison = KernelComparison {
            left,
            right,
            added: vec!["src/added.cpp".into()],
            removed: vec!["src/removed.cpp".into()],
            changed: vec!["src/changed.cpp".into()],
            identical: 17,
        };
        let installed = kernel_fixture("E:/fixtures/Steam", "installed");
        let report = build_report_with_context(None, None, Some(comparison), Some(installed));
        assert_eq!(report["schemaVersion"], 1);
        assert_eq!(report["runtimeCompatibilityVerified"], false);
        assert_eq!(report["comparison"]["left"]["version"], "1.2.3");
        assert_eq!(report["comparison"]["right"]["kind"], "source");
        assert_eq!(report["comparison"]["added"], json!(["src/added.cpp"]));
        assert_eq!(report["comparison"]["removed"], json!(["src/removed.cpp"]));
        assert_eq!(report["comparison"]["changed"], json!(["src/changed.cpp"]));
        assert_eq!(report["comparison"]["identical"], 17);
        assert_eq!(report["installedKernel"]["kind"], "installed");
        assert_eq!(report["steam"], Value::Null);
        assert_eq!(report["kernel"], Value::Null);
    }

    #[test]
    fn installation_context_preserves_full_state_and_legacy_fields() {
        let installed = kernel_fixture("E:/fixtures/Steam", "installed");
        let installation = installation_fixture("E:/fixtures/Steam");
        let report = build_report_with_installation_context(
            None,
            None,
            None,
            Some(installed),
            Some(installation),
        );
        assert_eq!(report["schemaVersion"], 1);
        assert_eq!(report["runtimeCompatibilityVerified"], false);
        assert_eq!(report["installedKernel"]["kind"], "installed");
        assert_eq!(report["kernelInstallation"]["status"], "enabled");
        assert_eq!(report["kernelInstallation"]["injectionMethod"], "dwmapi");
        assert_eq!(report["kernelInstallation"]["version"], "1.5.4.1");
        assert_eq!(
            report["kernelInstallation"]["files"][0]["name"],
            "OpenSteamTool.dll"
        );
        assert_eq!(
            report["kernelInstallation"]["remnants"],
            json!(["bin/dwmapi.dll"])
        );
        assert_eq!(
            report["kernelInstallation"]["messageKey"],
            "diagnostic.kernel_installation.enabled.body"
        );
    }

    #[test]
    fn legacy_context_keeps_new_installation_field_nullable() {
        let report = build_report_with_context(None, None, None, None);
        assert_eq!(report["kernelInstallation"], Value::Null);
    }

    #[test]
    fn comparison_sides_lists_and_installed_config_are_all_redacted() {
        let mut installed = kernel_fixture("c:/USERS/玩家/Steam", "installed");
        installed.config = Some(crate::ConfigSummary {
            path: "C:\\Users\\玩家\\Steam\\opensteamtool.toml".into(),
            valid: true,
            sections: vec!["log".into()],
            unknown_sections: vec![],
            error: None,
            error_key: String::new(),
            error_args: std::collections::BTreeMap::new(),
        });
        installed.diagnostics.push(crate::Diagnostic {
            code: "fixture".into(),
            severity: "info".into(),
            title: "Fixture".into(),
            detail: "Inspect c:/users/玩家/Steam/logs".into(),
            message_key: String::new(),
            message_args: std::collections::BTreeMap::new(),
        });
        let comparison = KernelComparison {
            left: kernel_fixture("C:\\Users\\玩家\\original", "source"),
            right: kernel_fixture("c:/users/玩家/fork", "source"),
            added: vec!["C:/Users/玩家/new.cpp".into()],
            removed: vec!["C:/Users/玩家/old.cpp".into()],
            changed: vec!["C:/Users/玩家/changed.cpp".into()],
            identical: 0,
        };
        let mut report = build_report_with_context(None, None, Some(comparison), Some(installed));
        redact_with_homes(&mut report, &["C:\\Users\\玩家".into()]);
        assert_eq!(
            report["comparison"]["left"]["path"],
            "%USERPROFILE%\\original"
        );
        assert_eq!(report["comparison"]["right"]["path"], "%USERPROFILE%/fork");
        assert_eq!(report["comparison"]["added"][0], "%USERPROFILE%/new.cpp");
        assert_eq!(report["comparison"]["removed"][0], "%USERPROFILE%/old.cpp");
        assert_eq!(
            report["comparison"]["changed"][0],
            "%USERPROFILE%/changed.cpp"
        );
        assert_eq!(report["installedKernel"]["path"], "%USERPROFILE%/Steam");
        assert_eq!(
            report["installedKernel"]["config"]["path"],
            "%USERPROFILE%\\Steam\\opensteamtool.toml"
        );
        assert_eq!(
            report["installedKernel"]["diagnostics"][0]["detail"],
            "Inspect %USERPROFILE%/Steam/logs"
        );
        assert_eq!(report["runtimeCompatibilityVerified"], false);
    }

    #[test]
    fn installation_path_diagnostics_and_remnants_are_all_redacted() {
        let mut installation = installation_fixture("C:\\Users\\玩家\\Steam");
        installation.remnants = vec![
            "C:/Users/玩家/Steam/bin/dwmapi.dll".into(),
            "C:\\Users\\玩家\\Steam\\version.dll.off".into(),
        ];
        installation.diagnostics[0].detail = "Inspect c:/users/玩家/Steam/opensteamtool.log".into();
        let mut report =
            build_report_with_installation_context(None, None, None, None, Some(installation));
        redact_with_homes(&mut report, &["C:\\Users\\玩家".into()]);
        assert_eq!(report["kernelInstallation"]["path"], "%USERPROFILE%\\Steam");
        assert_eq!(
            report["kernelInstallation"]["messageArgs"]["path"],
            "%USERPROFILE%\\Steam"
        );
        assert_eq!(
            report["kernelInstallation"]["diagnostics"][0]["detail"],
            "Inspect %USERPROFILE%/Steam/opensteamtool.log"
        );
        assert_eq!(
            report["kernelInstallation"]["diagnostics"][0]["messageArgs"]["path"],
            "%USERPROFILE%\\Steam"
        );
        assert_eq!(
            report["kernelInstallation"]["remnants"],
            json!([
                "%USERPROFILE%/Steam/bin/dwmapi.dll",
                "%USERPROFILE%\\Steam\\version.dll.off"
            ])
        );
    }
}
