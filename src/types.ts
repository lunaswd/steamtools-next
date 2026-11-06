export interface SteamInstallation { path:string; executable:string; version:string|null; clientBuild:string|null; running:boolean; detectionSource:string }
export interface SteamLibrary { path:string; exists:boolean; appCount:number; sizeBytes:number }
export interface SteamApp { appid:number; name:string; installDir:string; manifestPath:string; libraryPath:string; sizeBytes:number; stateFlags:number; lastUpdated:number; lastPlayed?:number }
export interface SteamSnapshot { capturedAt:number; locale:string; steam:SteamInstallation|null; libraries:SteamLibrary[]; apps:SteamApp[]; warnings:string[] }

export interface Diagnostic { code:string; severity:'info'|'warning'|'error'|'success'; title:string; detail:string; messageKey?:string; messageArgs?:Record<string,string> }
export interface KernelFile { name:string; sizeBytes:number; sha256:string }
export type KernelInstallationStatus = 'notInstalled'|'enabled'|'disabled'|'conflict'|'incomplete';
export type KernelInjectionMethod = 'none'|'dwmapi'|'version'|'both';
export interface KernelInstallation {
  path:string; status:KernelInstallationStatus; injectionMethod:KernelInjectionMethod; version:string|null;
  versionSource:'peResource'|'artifactManifest'|'unknown'; installedSha256:string|null;
  artifactVersion:string|null; artifactSha256:string|null; hashMatch:boolean|null; runtimeLoadVerified:boolean;
  files:KernelFile[]; remnants:string[]; messageKey:string; messageArgs:Record<string,string>; diagnostics:Diagnostic[]
}

export interface LuaDirectoryLayout { steamRoot:string; luaRoot:string; disabledRoot:string; configuredPaths:string[]; source:string; warnings:string[] }
export interface LuaLibraryEntry { appId:number; fileName:string; enabled:boolean; conflict:boolean; activePath:string|null; disabledPath:string|null; sizeBytes:number; sha256:string; appIds:number[]; pinnedManifestCount:number; pinnedDepotIds:number[] }
export interface LuaLibrarySnapshot { layout:LuaDirectoryLayout; entries:LuaLibraryEntry[]; warnings:string[] }
export interface ManagedWrite { destination:string; backupPath:string|null; sizeBytes:number; sha256:string; replaced:boolean }
export interface LuaMutationReceipt { appId:number; action:string; source:string|null; destination:string|null; backupPaths:string[] }
export interface ImportIssue { path:string; reason:string }
export interface ImportSummary { luaCount:number; binCount:number; manifestCount:number; imported:ManagedWrite[]; skipped:ImportIssue[]; failed:ImportIssue[] }
export type ArchiveFormat = 'zip'|'tar'|'tarGz';
export interface ArchiveImportIssue { archive:string; entry:string; reason:string }
export interface ArchiveResult { archive:string; format:ArchiveFormat|null; extractedFiles:number; extractedBytes:number; success:boolean; error:string|null }
export interface ArchiveImportSummary { archives:ArchiveResult[]; extractedFiles:number; extractedBytes:number; skipped:ArchiveImportIssue[]; failed:ArchiveImportIssue[]; import:ImportSummary }

export interface CatalogGame { appId:number; name:string; coverUrl:string; coverCandidates:string[]; releaseDate:string; followers:number; source:string }
export interface CatalogRankings { popular:CatalogGame[]; recommended:CatalogGame[] }
export interface LuaGenerationOptions { includeDlc:boolean; pinManifest:boolean; fetchNames:boolean }
export interface LuaGenerationResult { appId:number; appName:string; lua:string; depotCount:number; dlcCount:number; pinnedManifestCount:number; skippedKeyless:number[]; incompleteDlc?:number[]; steps:string[] }
export interface KeyCacheUpdate { cacheDir:string; depotKeysPath:string; appTokensPath:string; depotKeysBytes:number; appTokensBytes:number; updatedAt:number }
export interface RemoteImportIssue { fileName:string; reason:string }
export interface RemoteImportResult { appId:number; imported:ManagedWrite[]; skipped:RemoteImportIssue[]; archiveBytes:number; luaFiles:number; attempts:number; steps:string[] }
export interface CatalogAddReceipt { generation:LuaGenerationResult; write:ManagedWrite; library:LuaLibrarySnapshot }

export type InjectionMethod = 'dwmapi'|'version';
export type KernelOperation = 'install'|'switchInjection'|'enable'|'disable'|'uninstall';
export type KernelFileActionKind = 'keep'|'create'|'replace'|'remove'|'move'|'restore';
export type ManagedKernelFileAction = 'created'|'replaced'|'removed'|'moved'|'restored';
export interface KernelReleaseArtifact { sourceUrl:string; releaseTag:string; version:string; sourceCommit:string; assetName:string; downloadUrl:string; archiveSha256:string; manifestSha256?:string; artifactSha256:string; artifactRoot:string; cacheHit:boolean }
export interface RemoteKernelInstallRequest { steamRoot:string; installExampleConfig:boolean; backupRoot:string|null; dryRun:boolean }
export interface RemoteKernelInstallReceipt { release:KernelReleaseArtifact; receipt:KernelOperationReceipt }
export interface KernelUninstallRequest { steamRoot:string; installReceipt:string|null; backupRoot:string|null; force:boolean; dryRun:boolean }
export interface KernelFileAction { kind:KernelFileActionKind; source:string|null; target:string; backup:string|null; sizeBytes:number|null; sha256:string|null; previousSha256:string|null }
export interface KernelOperationPlan { operationId:string; operation:KernelOperation; steamRoot:string; backupDir:string; previousState:KernelInstallation; expectedStatus:string; expectedInjectionMethod:string; steamRunning:boolean; alreadySatisfied:boolean; blockers:string[]; actions:KernelFileAction[]; artifactRoot:string|null; artifactVersion:string|null; artifactSha256:string|null }
export interface ManagedKernelFile { target:string; action:ManagedKernelFileAction; installedSha256:string|null; previousSha256:string|null; previousBackup:string|null }
export interface KernelOperationReceipt { schemaVersion:number; operationId:string; operation:KernelOperation; dryRun:boolean; changed:boolean; steamRoot:string; backupDir:string; receiptPath:string|null; previousState:KernelInstallation; resultingState:KernelInstallation; records:ManagedKernelFile[]; plan:KernelOperationPlan }

export interface ManifestRequest { appId:number; depotId:number; manifestId:number }
export interface ManifestLogCursor { offset:number; prefixSha256:string; prefixLen:number; partialLine:string; seenKeys:string[] }
export interface ManifestPoll { logPath:string; reset:boolean; bytesRead:number; requests:ManifestRequest[]; malformedLines:number }
export interface ManifestAttempt { stage:string; source:string; status:number|null; success:boolean; detail:string }
export interface ManifestDownloadReceipt { request:ManifestRequest; source:string; destination:string; sizeBytes:number; sha256:string; replaced:boolean; backupPath:string|null; attempts:ManifestAttempt[] }
export interface ManifestBatchFailure { request:ManifestRequest; error:string }
export interface ManifestBatchResult { poll:ManifestPoll; downloaded:ManifestDownloadReceipt[]; failed:ManifestBatchFailure[] }
export interface ManifestPollResponse { cursor:ManifestLogCursor; batch:ManifestBatchResult }

export interface Settings {
  language:string; theme:string; accentColor:'steamBlue'|'mint'|'violet'; compactMode:boolean; showCovers:boolean; reduceMotion:boolean; steamPath:string|null;
  /** Legacy setting; ignored by the management UI. */ kernelPath:string|null;
  /** Legacy setting; ignored by remote installation. */ kernelArtifactPath:string|null; kernelReleaseSource?:string|null; injectionMethod:InjectionMethod; downloadMode:'depotKeys'|'remote';
  includeDlc:boolean; pinManifest:boolean; fetchNameComments:boolean; autoRefreshKeyCache:boolean;
  showRanking:boolean; autoRefreshLibrary:boolean; enableLogging:boolean;
}
export interface Environment { locale:string; version:string; defaultKernelPath:string|null; defaultKernelArtifactPath:string|null; defaultKernelReleaseSource?:string; desktop:boolean }
export interface CommandError { code:string; messageKey:string; messageArgs:Record<string,string>; detail:string }

/** Native Steam preferences only; authentication material never crosses IPC. */
export interface SteamSavedAccount { steamId:string; accountName:string; personaName:string; isCurrent:boolean; lastLoginAt:number|null; canAutoLogin:boolean }
export interface SteamAccountState { steamRoot:string; running:boolean; currentSteamId:string|null; currentAccountName:string|null; accounts:SteamSavedAccount[]; identitySource:string; warnings:string[] }
export interface SteamLifecycleReceipt { operation:'restart'|'relogin'|'switchAccount'; steamRoot:string; previousRunning:boolean; launchRequested:boolean; processId:number|null; requiresNativeLogin:boolean; selectedSteamId:string|null; backupPath:string|null; warnings:string[] }

export interface SteamAchievement {
  id:string; displayName:string; description:string; icon:string; iconGray:string;
  unlocked:boolean|null; unlockTime:number|null; hidden:boolean; protected:boolean;
}
export interface AchievementCatalog {
  appId:number; requestedLanguage:string; resolvedLanguage:string; source:string;
  connected:boolean; writable:boolean; achievements:SteamAchievement[]; error:string|null; sessionErrorCode?:string|null;
}
export interface AchievementChange { id:string; unlock:boolean }
export interface AchievementChangeResult extends AchievementChange { success:boolean; error:string|null; localVerified?:boolean }
export interface AchievementApplyResult {
  appId:number; confirmed:boolean; results:AchievementChangeResult[]; catalog:AchievementCatalog;
  localVerified?:boolean; serverAcknowledged?:boolean; saveWarning?:string|null;
}

export interface GameCover { appId:number; dataUrl:string; source:string }

export interface CurrentSessionGame { appId:number; name:string; installed:boolean; subscribed:boolean|null }
export interface CurrentSteamSession {
  connected:boolean; source:string; accountLabel:string; accountFingerprint:string; steamPath:string;
  games:CurrentSessionGame[]; capabilities:{subscriptionChecks:boolean;installedDepotKeys:boolean;freshDepotKeyRequests:boolean;localManifests:boolean;achievementSchema:boolean};
  warnings:string[]; error:string|null;
}
export interface ExtractedDepot { depotId:number; manifestId:string|null; keyAvailable:boolean; keySource:string|null; missingReason:string|null }
export interface ExtractionAsset { kind:'achievementSchema'|'manifest'; filename:string; sourcePath:string; sizeBytes:number; sha256:string }
export interface CurrentSessionExtraction {
  session:CurrentSteamSession; appId:number; appName:string; subscribed:boolean; scope:'installedDepots';
  depots:ExtractedDepot[]; keyedDepotCount:number; lua:string; assets:ExtractionAsset[]; complete:boolean; warnings:string[];
}
export interface ExtractionExportReceipt { appId:number; destinationDirectory:string; files:{name:string;kind:string;sizeBytes:number;sha256:string}[]; keyedDepotCount:number; warnings:string[] }
