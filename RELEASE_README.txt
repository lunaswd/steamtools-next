SteamTools Next — Windows x64

This package contains the desktop tool only. It does not contain a user's game library, saved Steam credentials, personal screenshots or the separate kernel.

Getting started
1. Start SteamTools Next.exe.
2. Confirm the Steam directory in Settings.
3. Kernel management offers only Install and Uninstall. The release source is fixed in the application and does not require a local DLL/source directory.
4. Exit Steam before kernel file operations. Existing configuration and operation backups are preserved.

Steam account menu
The bottom-left account button provides local saved-account summaries, Re-login and Restart Steam.
- Restart requests Steam's own graceful shutdown, waits for exit, then starts Steam again.
- Re-login uses Steam's native account-selection/login interface. Enter any password or Steam Guard confirmation only inside Steam.
- Confirm before proceeding: games or downloads may be interrupted. Starting a process is not proof of a signed-in session.

Updates
The program checks public GitHub release metadata for tool and kernel updates.
- Kernel updates use the Install action and the independently published kernel package.
- A confirmed tool update verifies the download, exits the old program, replaces the package files and automatically starts the new version. Replacement failures restore the prior files.
- Failed checks or downloads keep the current version usable; they do not silently select an unrelated source.

Source and licenses
Tool: https://github.com/lunaswd/steamtools-next
Kernel and complete corresponding kernel source: https://github.com/lunaswd/steamtools-next-kernel
See LICENSE.txt, THIRD-PARTY-NOTICES.txt and licenses.txt. SHA256SUMS.txt contains package file checksums.

Runtime boundaries
File installation and hash matching do not prove Steam module loading or game compatibility. Browser preview data is synthetic. Account/resource operations use the current Steam client and are not a separate stored-password login service.
