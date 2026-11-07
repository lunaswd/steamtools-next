//! Exercise the actual generated capability authority and IPC command dispatch.
//! Empty target input reaches validation without networking or Steam writes.
use tauri::{ipc::InvokeBody, test::MockRuntime, Manager};

fn app() -> tauri::App<MockRuntime> {
    app_with_context(tauri::generate_context!())
}

fn app_with_context(context: tauri::Context<MockRuntime>) -> tauri::App<MockRuntime> {
    tauri::test::mock_builder()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            super::get_environment_command,
            super::steam_account_state_command,
            super::restart_steam_command,
            super::relogin_steam_command,
            super::switch_saved_steam_account_command,
            super::authorization_session_command,
            super::inspect_authorization_ticket_command,
            super::extract_authorization_ticket_command,
            super::import_authorization_ticket_command,
            super::install_remote_kernel_command
        ])
        .build(context)
        .expect("Could not build the real generated app ACL in the mock runtime")
}

#[test]
fn missing_app_command_grants_reproduce_acl_interception_before_validation() {
    // Reproduce the omitted-permission seam without editing the real capability.
    // Merely registering a command in the handler never bypasses an app ACL.
    let mut context = tauri::generate_context!();
    *context.runtime_authority_mut() = tauri::runtime_authority!(
        Default::default(),
        tauri::utils::acl::resolved::Resolved {
            has_app_acl: true,
            ..Default::default()
        }
    );
    let app = app_with_context(context);
    let main = webview(&app, "main");
    let error = invoke(
        &main,
        "install_remote_kernel_command",
        "http://tauri.localhost",
    )
    .expect_err("Missing grants must intercept the registered command");
    assert!(
        error
            .as_str()
            .is_some_and(|text| text.contains("not allowed")),
        "Expected the actual ACL interception rather than input validation: {error}"
    );
    assert!(error.get("code").is_none());
}

fn webview(app: &tauri::App<MockRuntime>, label: &str) -> tauri::WebviewWindow<MockRuntime> {
    app.get_webview_window(label).unwrap_or_else(|| {
        tauri::WebviewWindowBuilder::new(app, label, Default::default())
            .build()
            .expect("Could not create the isolated mock webview")
    })
}

fn invoke(
    webview: &tauri::WebviewWindow<MockRuntime>,
    command: &str,
    origin: &str,
) -> Result<tauri::ipc::InvokeResponseBody, serde_json::Value> {
    tauri::test::get_ipc_response(
        webview,
        tauri::webview::InvokeRequest {
            cmd: command.into(),
            callback: tauri::ipc::CallbackFn(0),
            error: tauri::ipc::CallbackFn(1),
            url: origin.parse().unwrap(),
            body: InvokeBody::Json(
                serde_json::json!({"steamPath":"", "steamId":"1", "appId":42,"sourcePath":"","expectedSteamId":"1","expectedSha256":"","destination":"","overwrite":false, "request": {
                    "steamRoot": "",
                    "installExampleConfig": false,
                    "backupRoot": null,
                    "dryRun": true
                }}),
            ),
            headers: Default::default(),
            invoke_key: tauri::test::INVOKE_KEY.to_string(),
        },
    )
}

#[test]
fn remote_kernel_commands_reach_validation_through_real_local_ipc_acl() {
    let app = app();
    let main = webview(&app, "main");
    for command in [
        "install_remote_kernel_command",
        "steam_account_state_command",
        "restart_steam_command",
        "relogin_steam_command",
        "switch_saved_steam_account_command",
        "authorization_session_command",
        "inspect_authorization_ticket_command",
        "extract_authorization_ticket_command",
        "import_authorization_ticket_command",
    ] {
        let error = invoke(&main, command, "http://tauri.localhost")
            .expect_err("Empty Steam path must be rejected by input validation");
        assert_eq!(
            error["code"], "invalid_input",
            "{command} was intercepted before command validation: {error}"
        );
        assert_eq!(error["messageKey"], "error.pathRequired");
    }
}

#[test]
fn remote_kernel_commands_stay_denied_to_remote_web_origins() {
    let app = app();
    let main = webview(&app, "main");
    for command in [
        "install_remote_kernel_command",
        "steam_account_state_command",
        "restart_steam_command",
        "relogin_steam_command",
        "switch_saved_steam_account_command",
        "authorization_session_command",
        "inspect_authorization_ticket_command",
        "extract_authorization_ticket_command",
        "import_authorization_ticket_command",
    ] {
        let error = invoke(&main, command, "https://example.invalid")
            .expect_err("App command permissions must not extend to a remote origin");
        assert!(
            error
                .as_str()
                .is_some_and(|text| text.contains("not allowed")),
            "Unexpected rejection: {error}"
        );
    }
}

#[test]
fn remote_kernel_commands_stay_denied_to_other_windows() {
    let app = app();
    let other = webview(&app, "untrusted-fixture");
    let error = invoke(
        &other,
        "install_remote_kernel_command",
        "http://tauri.localhost",
    )
    .expect_err("Only the main window receives installation capability");
    assert!(
        error
            .as_str()
            .is_some_and(|text| text.contains("not allowed")),
        "Unexpected rejection: {error}"
    );
}

#[test]
fn environment_command_returns_through_real_local_ipc_acl() {
    let app = app();
    let main = webview(&app, "main");
    let result = invoke(&main, "get_environment_command", "http://tauri.localhost")
        .expect("Existing app commands must retain access");
    let result: serde_json::Value = result.deserialize().unwrap();
    assert_eq!(result["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(result["defaultKernelArtifactPath"], serde_json::Value::Null);
}
