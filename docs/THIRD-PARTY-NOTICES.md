# Third-party notices

This document covers the independently distributed SteamTools Next desktop utility. It contains no user game-library records, account data or personal filesystem paths.

## SteamTools Next

Original project code is distributed under the MIT License in `LICENSE`. This license does not replace the licenses of separately downloaded software or dependencies.

## OpenSteamTool

The OpenSteamTool kernel is a separate GPL-3.0 component from https://github.com/lunaswd/steamtools-next-kernel. The GUI package does not bundle or link the kernel DLLs. It downloads and deploys the user-selected published release; that release must retain GPL and dependency notices and provide the complete corresponding source bound to its manifest `sourceCommit`.

Managing that separate installation does not remove the kernel's license obligations. The source repository and every binary release must retain the required license texts and matching build/source materials.

## Fluent Steam Lua / SteamLuaManager

The tool's file-management, catalog, manifest and kernel-management workflows were studied and reimplemented in modified Rust/TypeScript code. This is not the original Fluent implementation, and the original authors are not represented as authors of this rewritten tool.

Original license:

```text
zlib License

Copyright (c) 2024 SteamLuaManager

This software is provided 'as-is', without any express or implied
warranty. In no event will the authors be held liable for any damages
arising from the use of this software.

Permission is granted to anyone to use this software for any purpose,
including commercial applications, and to alter it and redistribute it
freely, subject to the following restrictions:

1. The origin of this software must not be misrepresented; you must not
   claim that you wrote the original software. If you use this software
   in a product, an acknowledgment in the product documentation would be
   appreciated but is not required.

2. Altered source versions must be plainly marked as such, and must not be
   misrepresented as being the original software.

3. This notice may not be removed or altered from any source
   distribution.
```

## SteamAchievementManager / SAM.API

The Steam client interface layout, callbacks and binary KeyValue parsing reference the modified SAM.API workflow. The project does not ship the SAM GUI or Valve client DLLs.

Upstream: https://github.com/gibbed/SteamAchievementManager

```text
zlib License

Copyright (c) 2024 Rick (rick 'at' gibbed 'dot' us)

This software is provided 'as-is', without any express or implied
warranty. In no event will the authors be held liable for any damages
arising from the use of this software.

Permission is granted to anyone to use this software for any purpose,
including commercial applications, and to alter it and redistribute it
freely, subject to the following restrictions:

1. The origin of this software must not be misrepresented; you must not
   claim that you wrote the original software. If you use this software
   in a product, an acknowledgment in the product documentation would
   be appreciated but is not required.

2. Altered source versions must be plainly marked as such, and must not
   be misrepresented as being the original software.

3. This notice may not be removed or altered from any source
   distribution.
```

## Valve / Steam

Steam, Steamworks, Steam Store and associated names or assets belong to Valve Corporation. SteamTools Next is not affiliated with Valve. Local client files and remote service data do not obtain a new license through this project.

Runtime service sources include Steam Store/Web API/CDNs and the catalog/manifest providers encoded in the source. Their availability and terms are independent of the MIT license. User configuration, API credentials, Steam Guard data and depot key caches are not part of the public repository or GUI package.

## Rust and npm dependencies

`collect_licenses.py` reads public dependency metadata and existing license texts from the local Cargo/npm dependency caches and exact-version override texts. Releases consolidate the required notices into `licenses.txt`; applicable corresponding third-party sources are provided in the separate source asset. These dependency license texts and authorship notices must not be removed.

The packaging process does not scan Steam account directories, user game resources or reference screenshots. Version/source identity, content integrity and runtime compatibility remain separate release checks.
