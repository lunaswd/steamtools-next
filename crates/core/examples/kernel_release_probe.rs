//! Real HTTPS acquisition and deployment checks in a newly created fixture only.
//! Usage: cargo run -p steam-tools-core --example kernel_release_probe -- CACHE FIXTURE REPORT
use anyhow::{ensure, Context, Result};
use std::{fs, path::PathBuf};
use steam_tools_core::{
    acquire_kernel_release, install_kernel_release, plan_kernel_release_install, uninstall_kernel,
    InjectionMethod, KernelInstallRequest, KernelUninstallRequest, DEFAULT_KERNEL_RELEASE_SOURCE,
};

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    ensure!(
        args.len() == 3,
        "Expected absolute CACHE FIXTURE REPORT paths"
    );
    ensure!(
        args.iter().all(|path| path.is_absolute()),
        "Paths must be absolute"
    );
    ensure!(
        !args[1].exists(),
        "Fixture already exists; preserved without changes"
    );
    ensure!(
        !args[2].exists(),
        "Report already exists; preserved without changes"
    );
    let release = acquire_kernel_release(DEFAULT_KERNEL_RELEASE_SOURCE, &args[0])?;
    let cached = acquire_kernel_release(DEFAULT_KERNEL_RELEASE_SOURCE, &args[0])?;
    ensure!(
        cached.cache_hit,
        "Second acquisition must reuse the verified ZIP cache"
    );
    ensure!(
        cached.artifact_sha256 == release.artifact_sha256,
        "Cached DLL identity changed"
    );
    fs::create_dir(&args[1]).context("Could not create a new isolated deployment fixture")?;
    for name in ["steam.exe", "steamclient64.dll", "steamui.dll"] {
        fs::write(
            args[1].join(name),
            b"SteamTools Next deployment fixture; not executable",
        )?;
    }
    let config = b"[lua]\npaths = [\"config/lua\"]\n";
    fs::write(args[1].join("opensteamtool.toml"), config)?;
    let mut request = KernelInstallRequest {
        steam_root: args[1].clone(),
        artifact_root: PathBuf::from(&release.artifact_root),
        injection_method: InjectionMethod::Dwmapi,
        install_example_config: true,
        backup_root: None,
        dry_run: true,
    };
    let plan = plan_kernel_release_install(&request, &release)?;
    ensure!(plan.blockers.is_empty(), "Fixture preflight was blocked");
    let dry_run = install_kernel_release(&request, &release)?;
    ensure!(
        dry_run.dry_run && !dry_run.changed,
        "Preview wrote deployment files"
    );
    ensure!(
        !args[1].join("OpenSteamTool.dll").exists(),
        "Preview installed a DLL"
    );
    request.dry_run = false;
    let installed = install_kernel_release(&request, &release)?;
    ensure!(
        installed.resulting_state.hash_match == Some(true),
        "Installed DLL hash mismatch"
    );
    ensure!(
        installed.resulting_state.version.as_deref() == Some(release.version.as_str()),
        "Installed version mismatch"
    );
    ensure!(
        args[1].join("dwmapi.dll").is_file() && !args[1].join("version.dll").exists(),
        "Injection entries conflict"
    );
    ensure!(
        fs::read(args[1].join("opensteamtool.toml"))? == config,
        "Existing config was replaced"
    );
    let repeated = install_kernel_release(&request, &cached)?;
    ensure!(
        !repeated.changed && repeated.resulting_state.hash_match == Some(true),
        "Idempotent install lost verification"
    );
    let uninstalled = uninstall_kernel(&KernelUninstallRequest {
        steam_root: args[1].clone(),
        install_receipt: installed.receipt_path.as_ref().map(PathBuf::from),
        backup_root: None,
        force: false,
        dry_run: false,
    })?;
    ensure!(
        uninstalled.resulting_state.status == "notInstalled",
        "Fixture uninstall incomplete"
    );
    ensure!(
        fs::read(args[1].join("opensteamtool.toml"))? == config,
        "Uninstall changed existing config"
    );
    let report = serde_json::json!({
        "schemaVersion": 1,
        "source": DEFAULT_KERNEL_RELEASE_SOURCE,
        "scope": "real HTTPS acquisition; isolated fixture deployment only",
        "runtimeLoadVerified": false,
        "release": release,
        "cachedRelease": cached,
        "plan": plan,
        "dryRun": dry_run,
        "installed": installed,
        "idempotent": repeated,
        "uninstalled": uninstalled,
    });
    fs::write(&args[2], serde_json::to_vec_pretty(&report)?)?;
    println!(
        "Verified remote release and isolated deployment: {}",
        args[2].display()
    );
    Ok(())
}
