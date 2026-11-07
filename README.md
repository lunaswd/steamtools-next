# SteamTools Next

SteamTools Next is a Windows desktop Steam-management utility built with Rust, Tauri 2 and TypeScript. This repository contains tool source only: it does not contain a user's game library, saved accounts, credentials, runtime logs or personal screenshots.

## Current interface

- **Kernel:** install or uninstall the independently released OpenSteamTool kernel. The GitHub source is built in and is not editable in the application. Source-workspace inspection controls are not part of the user interface.
- **Steam account menu:** the bottom-left menu shows local saved-account summaries and offers confirmed switching between saved accounts, native Steam re-login and a graceful Steam restart. Passwords and Steam Guard confirmation stay in Steam's own interface; the tool does not collect or persist them.
- **Tool updates:** published GitHub releases are checked without sending library or account data. A user-confirmed update downloads verified files, exits the old process, applies the update and automatically starts the new version. Failed replacement is rolled back.
- **Library and imports:** manage Lua files, import related files and supported archives, and inspect Steam's locally installed applications.
- **Achievements and session resources:** use the current Steam client session, distinguish unknown state from confirmed state, and keep local readback separate from server acknowledgement.
- **Settings:** language, appearance and Steam-directory preferences. Kernel source and injection layout are internal defaults, not editable source paths.

The browser preview uses explicitly synthetic examples. It does not represent a real account or a real user's game library.

## Build

Run the following from the repository root on Windows with Node.js, Rust and WebView2 installed:

```powershell
npm.cmd ci
npm.cmd test
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
npm.cmd run desktop:build
```

For public binaries, use compiler path remapping so the executable does not retain a builder's personal filesystem location. The GitHub workflow configures this automatically.

## Release

Source pushes trigger the Windows build-and-release workflow. Each release binds its version, source commit, archive size and SHA-256 to `tool-release.json`. The kernel has its own repository and corresponding-source release:

- Tool: https://github.com/lunaswd/SteamTools-Next
- Kernel: https://github.com/lunaswd/steamtools-next-kernel

Releases contain a GUI ZIP with the program, usage information, licenses and checksums. Kernel DLLs and user data are not bundled in that ZIP. See `scripts/package_release.py --help` for the source-bound packaging interface.

## Publication privacy

Public source export is an explicit allowlist. The repository excludes build caches, application data, game archives, account VDF files, credentials, validation artifacts and screenshots. `scripts/audit_public_source.py` checks tracked source before release and reports only paths and issue categories, never the secret text it detects.

```powershell
python scripts/audit_public_source.py
python -m unittest discover -s scripts -p "test_*.py"
```

The bundled bitmap/vector assets are reviewed tool icons. Account names and game data are read only at runtime on the user's computer. GitHub update checks request fixed public release metadata and do not upload those local records.

## Integrity and limitations

Archive/file checksums, a successful file deployment and Steam runtime compatibility are different forms of evidence. Installing the kernel does not by itself prove that Steam loaded it or that a game is compatible. Steam restart and account switching can interrupt games or downloads and therefore require an explicit user confirmation.

The tool is not affiliated with Valve. Project source is MIT-licensed; separate kernels and dependencies retain their own licenses. Read `docs/THIRD-PARTY-NOTICES.md`.

## Private authorization tickets

The Authorization Tickets page manages Steam AppTicket/EncryptedAppTicket text files compatible with Fluent. Extraction and import require the current signed-in account and a subscribed AppID. Only metadata crosses the GUI boundary; ticket bytes stay in private local files and the selected current-user registry values. Files are structure/identity checked, not independently signature-verified. A verified registry write does not establish Denuvo/game authorization. Existing Lua ticket statements are not modified and may override registry values later; inspect and resolve such conflicts before using an imported ticket. Never upload or share ticket files.
