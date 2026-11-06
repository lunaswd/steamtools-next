// Static diagnostic translation contract; contains no runtime or user data.
export default {
  "diagnostic.detail.unavailable": {
    "template": "Detailed localized information is unavailable for this older or unsupported diagnostic. The original technical detail remains available separately.",
    "args": [],
    "argumentDescriptions": {}
  },
  "diagnostic.reason.unavailable": {
    "template": "The detailed error reason is unavailable.",
    "args": [],
    "argumentDescriptions": {}
  },
  "diagnostic.file_unreadable.body": {
    "template": "{filename}: {reason}",
    "args": [
      "filename",
      "reason"
    ],
    "argumentDescriptions": {
      "filename": "Inspected filename or bounded relative filename; not file contents.",
      "reason": "Rendered from messageArgs.reasonKey using the same messageArgs map; one level only."
    }
  },
  "diagnostic.binary_architecture.body": {
    "template": "{filename}: {architecture}. Only the on-disk PE header was read.",
    "args": [
      "filename",
      "architecture"
    ],
    "argumentDescriptions": {
      "filename": "Inspected filename or bounded relative filename; not file contents.",
      "architecture": "Technical PE architecture identifier, e.g. x86-64."
    }
  },
  "diagnostic.binary_architecture.unknown": {
    "template": "{filename}: the PE machine type is not recognized. Only the on-disk PE header was read.",
    "args": [
      "filename"
    ],
    "argumentDescriptions": {
      "filename": "Inspected filename or bounded relative filename; not file contents."
    }
  },
  "diagnostic.binary_header_invalid.body": {
    "template": "{filename}: {reason}",
    "args": [
      "filename",
      "reason"
    ],
    "argumentDescriptions": {
      "filename": "Inspected filename or bounded relative filename; not file contents.",
      "reason": "Rendered from messageArgs.reasonKey using the same messageArgs map; one level only."
    }
  },
  "diagnostic.binary_version.body": {
    "template": "{filename}: {version} from the file-version resource. This is not an authenticated release or load-state check.",
    "args": [
      "filename",
      "version"
    ],
    "argumentDescriptions": {
      "filename": "Inspected filename or bounded relative filename; not file contents.",
      "version": "Source-declared or PE file-resource version; not a runtime assertion."
    }
  },
  "diagnostic.config_unreadable.body": {
    "template": "{filename}: {reason}",
    "args": [
      "filename",
      "reason"
    ],
    "argumentDescriptions": {
      "filename": "Inspected filename or bounded relative filename; not file contents.",
      "reason": "Rendered from messageArgs.reasonKey using the same messageArgs map; one level only."
    }
  },
  "diagnostic.config_invalid.duplicate": {
    "template": "Duplicate TOML key or table at line {line}, column {column}; source values omitted.",
    "args": [
      "line",
      "column"
    ],
    "argumentDescriptions": {
      "line": "1-based source line.",
      "column": "1-based source column."
    }
  },
  "diagnostic.config_invalid.duplicate_unlocated": {
    "template": "Duplicate TOML key or table; source location unavailable and values omitted.",
    "args": [],
    "argumentDescriptions": {}
  },
  "diagnostic.config_invalid.syntax": {
    "template": "Invalid TOML syntax at line {line}, column {column}; source values omitted.",
    "args": [
      "line",
      "column"
    ],
    "argumentDescriptions": {
      "line": "1-based source line.",
      "column": "1-based source column."
    }
  },
  "diagnostic.config_invalid.syntax_unlocated": {
    "template": "Invalid TOML syntax; source location unavailable and values omitted.",
    "args": [],
    "argumentDescriptions": {}
  },
  "diagnostic.config_root_value.body": {
    "template": "OpenSteamTool settings normally belong to named tables. The unexpected root-level value is omitted.",
    "args": [],
    "argumentDescriptions": {}
  },
  "diagnostic.config_valid.body": {
    "template": "Configuration was parsed without executing code. Syntax validity is not runtime validation.",
    "args": [],
    "argumentDescriptions": {}
  },
  "diagnostic.config_unknown_sections.body": {
    "template": "{count} section(s) are outside the known source schema; values are omitted.",
    "args": [
      "count"
    ],
    "argumentDescriptions": {
      "count": "Decimal count."
    }
  },
  "diagnostic.config_type_mismatch.body": {
    "template": "{field} must have type {expected}; its value is omitted.",
    "args": [
      "field",
      "expected"
    ],
    "argumentDescriptions": {
      "field": "Known schema field identifier.",
      "expected": "Known schema type or supported option identifiers; never configured values."
    }
  },
  "diagnostic.config_timeout_range.body": {
    "template": "{field} should be an integer within {minimum}–{maximum} ms; its value is omitted.",
    "args": [
      "field",
      "minimum",
      "maximum"
    ],
    "argumentDescriptions": {
      "field": "Known schema field identifier.",
      "minimum": "Decimal lower bound.",
      "maximum": "Decimal upper bound."
    }
  },
  "diagnostic.config_template_placeholders.body": {
    "template": "{field} requires {required}; missing placeholders: {missing}. The configured URL is omitted.",
    "args": [
      "field",
      "required",
      "missing"
    ],
    "argumentDescriptions": {
      "field": "Known schema field identifier.",
      "required": "Required literal template placeholders.",
      "missing": "Missing known placeholder or interface identifiers; never arbitrary TOML values."
    }
  },
  "diagnostic.config_template_transport.body": {
    "template": "The configured metadata URL template is not HTTPS. No network request was made.",
    "args": [],
    "argumentDescriptions": {}
  },
  "diagnostic.config_lua_paths_type.body": {
    "template": "lua.paths should be an array of strings. External configured directories are not followed by this inspector.",
    "args": [],
    "argumentDescriptions": {}
  },
  "diagnostic.config_enum_value.body": {
    "template": "{field} should be one of {expected}; the configured value is omitted.",
    "args": [
      "field",
      "expected"
    ],
    "argumentDescriptions": {
      "field": "Known schema field identifier.",
      "expected": "Known schema type or supported option identifiers; never configured values."
    }
  },
  "diagnostic.installed_kernel_detected.body": {
    "template": "{path}: a regular OpenSteamTool.dll exists in the selected Steam root. Presence does not prove authenticity or that Steam loaded it. Source-tree versions were not used.",
    "args": [
      "path"
    ],
    "argumentDescriptions": {
      "path": "Selected local path; report exporters redact user-home paths."
    }
  },
  "diagnostic.installed_version_unknown.body": {
    "template": "{filename} has no readable file-version resource. No version was inferred from source files, example configuration, filenames or the selected maintenance baseline.",
    "args": [
      "filename"
    ],
    "argumentDescriptions": {
      "filename": "Inspected filename or bounded relative filename; not file contents."
    }
  },
  "diagnostic.inspection_read_only.body": {
    "template": "No DLL was loaded, no Lua was executed, no account database was read and no Steam file was changed.",
    "args": [],
    "argumentDescriptions": {}
  },
  "diagnostic.kernel_missing.body": {
    "template": "{path} does not exist. Select a source tree, installation directory, TOML or Lua file.",
    "args": [
      "path"
    ],
    "argumentDescriptions": {
      "path": "Selected local path; report exporters redact user-home paths."
    }
  },
  "diagnostic.binary_identity_unverified.body": {
    "template": "A filename and content hash do not prove a DLL’s origin, version, signature or compatibility. It was not loaded.",
    "args": [],
    "argumentDescriptions": {}
  },
  "diagnostic.steam_path_unset.body": {
    "template": "Select a Steam installation to inspect its client/UI hashes and matching local metadata.",
    "args": [],
    "argumentDescriptions": {}
  },
  "diagnostic.runtime_unverified.body": {
    "template": "Version detection, file fingerprints and matching metadata are separate from runtime validation. No injection or game-level test was performed.",
    "args": [],
    "argumentDescriptions": {}
  },
  "diagnostic.source_version.body": {
    "template": "{filename} declares OpenSteamTool {version}. This is a source declaration, not a verified release tag.",
    "args": [
      "filename",
      "version"
    ],
    "argumentDescriptions": {
      "filename": "Inspected filename or bounded relative filename; not file contents.",
      "version": "Source-declared or PE file-resource version; not a runtime assertion."
    }
  },
  "diagnostic.license_detected.gpl3": {
    "template": "GNU GPL version 3 text detected. Check rights-holder notices before redistribution; this does not distinguish only/or-later grants.",
    "args": [],
    "argumentDescriptions": {}
  },
  "diagnostic.license_detected.unknown": {
    "template": "A license file exists; no supported license identity was inferred automatically.",
    "args": [],
    "argumentDescriptions": {}
  },
  "diagnostic.example_configuration.body": {
    "template": "{filename} is a source example, not evidence that Steam is using this configuration.",
    "args": [
      "filename"
    ],
    "argumentDescriptions": {
      "filename": "Inspected filename or bounded relative filename; not file contents."
    }
  },
  "diagnostic.binary_present.body": {
    "template": "{filename} exists. Presence is not proof of origin, load state or compatibility; it was not loaded.",
    "args": [
      "filename"
    ],
    "argumentDescriptions": {
      "filename": "Inspected filename or bounded relative filename; not file contents."
    }
  },
  "diagnostic.metadata_source.fork": {
    "template": "Source references {repository} with the fork metadata layout. No upstream network request was made.",
    "args": [
      "repository"
    ],
    "argumentDescriptions": {
      "repository": "Recognized public repository identifier."
    }
  },
  "diagnostic.metadata_source.original": {
    "template": "Source references {repository} with the original metadata layout. No upstream network request was made.",
    "args": [
      "repository"
    ],
    "argumentDescriptions": {
      "repository": "Recognized public repository identifier."
    }
  },
  "diagnostic.metadata_source.unknown": {
    "template": "No recognized metadata upstream was inferred from this source file.",
    "args": [],
    "argumentDescriptions": {}
  },
  "diagnostic.lua_unreadable.body": {
    "template": "{filename}: {reason}",
    "args": [
      "filename",
      "reason"
    ],
    "argumentDescriptions": {
      "filename": "Inspected filename or bounded relative filename; not file contents.",
      "reason": "Rendered from messageArgs.reasonKey using the same messageArgs map; one level only."
    }
  },
  "diagnostic.kernel_identity_unknown.body": {
    "template": "No OpenSteamTool source version or named primary DLL was found. Configuration files alone do not prove installation.",
    "args": [],
    "argumentDescriptions": {}
  },
  "diagnostic.linked_path_skipped.body": {
    "template": "{path}: a symbolic link or reparse point was not followed.",
    "args": [
      "path"
    ],
    "argumentDescriptions": {
      "path": "Selected local path; report exporters redact user-home paths."
    }
  },
  "diagnostic.steam_fingerprint.body": {
    "template": "{filename}: SHA-256 {sha256} was acquired from the file on disk, not from a loaded process.",
    "args": [
      "filename",
      "sha256"
    ],
    "argumentDescriptions": {
      "filename": "Inspected filename or bounded relative filename; not file contents.",
      "sha256": "SHA-256 of the inspected Steam component."
    }
  },
  "diagnostic.steam_fingerprint_unavailable.body": {
    "template": "{filename}: {reason}",
    "args": [
      "filename",
      "reason"
    ],
    "argumentDescriptions": {
      "filename": "Inspected filename or bounded relative filename; not file contents.",
      "reason": "Rendered from messageArgs.reasonKey using the same messageArgs map; one level only."
    }
  },
  "diagnostic.metadata_missing.body": {
    "template": "{channel}/{component}: no local TOML for SHA-256 {sha256} was found at {path}. No remote check was made.",
    "args": [
      "channel",
      "component",
      "sha256",
      "path"
    ],
    "argumentDescriptions": {
      "channel": "pattern or ipc.",
      "component": "steamclient or steamui.",
      "sha256": "SHA-256 of the inspected Steam component.",
      "path": "Selected local path; report exporters redact user-home paths."
    }
  },
  "diagnostic.metadata_unreadable.body": {
    "template": "{channel}/{component} at {path}: {reason}",
    "args": [
      "channel",
      "component",
      "path",
      "reason"
    ],
    "argumentDescriptions": {
      "channel": "pattern or ipc.",
      "component": "steamclient or steamui.",
      "path": "Selected local path; report exporters redact user-home paths.",
      "reason": "Rendered from messageArgs.reasonKey using the same messageArgs map; one level only."
    }
  },
  "diagnostic.metadata_invalid.body": {
    "template": "{channel}/{component} at {path}: {reason}",
    "args": [
      "channel",
      "component",
      "path",
      "reason"
    ],
    "argumentDescriptions": {
      "channel": "pattern or ipc.",
      "component": "steamclient or steamui.",
      "path": "Selected local path; report exporters redact user-home paths.",
      "reason": "Rendered from messageArgs.reasonKey using the same messageArgs map; one level only."
    }
  },
  "diagnostic.metadata_match.body": {
    "template": "{channel}/{component}: the local filename matches SHA-256 {sha256} and minimum table checks passed. Completeness, authenticity and runtime behavior remain unverified.",
    "args": [
      "channel",
      "component",
      "sha256"
    ],
    "argumentDescriptions": {
      "channel": "pattern or ipc.",
      "component": "steamclient or steamui.",
      "sha256": "SHA-256 of the inspected Steam component."
    }
  },
  "diagnostic.metadata_structure_invalid.ipc": {
    "template": "{channel}/{component}: required IPC interface entries are absent or invalid: {missing}. Each required interface needs at least one valid funcHash method. No metadata was modified.",
    "args": [
      "channel",
      "component",
      "missing"
    ],
    "argumentDescriptions": {
      "channel": "pattern or ipc.",
      "component": "steamclient or steamui.",
      "missing": "Missing known placeholder or interface identifiers; never arbitrary TOML values."
    }
  },
  "diagnostic.metadata_structure_invalid.pattern": {
    "template": "{channel}/{component}: no valid hexadecimal function table with a nonzero rva or valid sig was found. No metadata was modified.",
    "args": [
      "channel",
      "component"
    ],
    "argumentDescriptions": {
      "channel": "pattern or ipc.",
      "component": "steamclient or steamui."
    }
  },
  "diagnostic.comparison_scope.body": {
    "template": "Compared {count} regular source/document/asset files ({bytes} bytes). Build outputs, account directories and {skippedLinks} linked paths were excluded.",
    "args": [
      "count",
      "bytes",
      "skippedLinks"
    ],
    "argumentDescriptions": {
      "count": "Decimal count.",
      "bytes": "Decimal byte count.",
      "skippedLinks": "Decimal count of omitted links."
    }
  },
  "diagnostic.reason.not_regular": {
    "template": "A regular file was expected.",
    "args": [],
    "argumentDescriptions": {}
  },
  "diagnostic.reason.linked_path": {
    "template": "Linked files, symbolic links and reparse-point paths are not inspected.",
    "args": [],
    "argumentDescriptions": {}
  },
  "diagnostic.reason.text_size_limit": {
    "template": "Text file exceeds the {limitBytes}-byte inspection limit.",
    "args": [
      "limitBytes"
    ],
    "argumentDescriptions": {
      "limitBytes": "Decimal byte limit."
    }
  },
  "diagnostic.reason.text_grew_limit": {
    "template": "Text file grew beyond the {limitBytes}-byte inspection limit.",
    "args": [
      "limitBytes"
    ],
    "argumentDescriptions": {
      "limitBytes": "Decimal byte limit."
    }
  },
  "diagnostic.reason.text_invalid_utf8": {
    "template": "Text file is not valid UTF-8.",
    "args": [],
    "argumentDescriptions": {}
  },
  "diagnostic.reason.file_size_limit": {
    "template": "File exceeds the {limitBytes}-byte fingerprint limit.",
    "args": [
      "limitBytes"
    ],
    "argumentDescriptions": {
      "limitBytes": "Decimal byte limit."
    }
  },
  "diagnostic.reason.file_grew_limit": {
    "template": "File grew beyond the {limitBytes}-byte fingerprint limit.",
    "args": [
      "limitBytes"
    ],
    "argumentDescriptions": {
      "limitBytes": "Decimal byte limit."
    }
  },
  "diagnostic.reason.file_disappeared": {
    "template": "File disappeared while being fingerprinted.",
    "args": [],
    "argumentDescriptions": {}
  },
  "diagnostic.reason.file_changed": {
    "template": "File changed while being fingerprinted; refresh the inspection.",
    "args": [],
    "argumentDescriptions": {}
  },
  "diagnostic.reason.io_not_found": {
    "template": "File or directory was not found (OS error {osCode}).",
    "args": [
      "osCode"
    ],
    "argumentDescriptions": {
      "osCode": "Numeric OS error code, or an em dash when the system supplied no numeric code."
    }
  },
  "diagnostic.reason.io_permission_denied": {
    "template": "Permission to read the file or directory was denied (OS error {osCode}).",
    "args": [
      "osCode"
    ],
    "argumentDescriptions": {
      "osCode": "Numeric OS error code, or an em dash when the system supplied no numeric code."
    }
  },
  "diagnostic.reason.io_invalid_data": {
    "template": "File data is invalid for the requested operation.",
    "args": [],
    "argumentDescriptions": {}
  },
  "diagnostic.reason.io_unexpected_eof": {
    "template": "File ended before the required data was read.",
    "args": [],
    "argumentDescriptions": {}
  },
  "diagnostic.reason.io_os_error": {
    "template": "File I/O failed with OS error {osCode}.",
    "args": [
      "osCode"
    ],
    "argumentDescriptions": {
      "osCode": "Numeric OS error code, or an em dash when the system supplied no numeric code."
    }
  },
  "diagnostic.reason.io_error": {
    "template": "File I/O failed; the system provided no numeric error code.",
    "args": [],
    "argumentDescriptions": {}
  },
  "diagnostic.reason.pe_header_incomplete": {
    "template": "File has no complete DOS/PE header.",
    "args": [],
    "argumentDescriptions": {}
  },
  "diagnostic.reason.pe_dos_signature": {
    "template": "File does not have a PE/DOS signature.",
    "args": [],
    "argumentDescriptions": {}
  },
  "diagnostic.reason.pe_header_offset": {
    "template": "PE header offset is outside the bounded range {minimum}–{maximum} bytes.",
    "args": [
      "minimum",
      "maximum"
    ],
    "argumentDescriptions": {
      "minimum": "Decimal lower bound.",
      "maximum": "Decimal upper bound."
    }
  },
  "diagnostic.reason.pe_header_truncated": {
    "template": "PE header is truncated.",
    "args": [],
    "argumentDescriptions": {}
  },
  "diagnostic.reason.pe_signature_invalid": {
    "template": "File does not have a valid PE signature.",
    "args": [],
    "argumentDescriptions": {}
  },
  "diagnostic.reason.toml_duplicate": {
    "template": "Duplicate TOML key or table at line {line}, column {column}; source values omitted.",
    "args": [
      "line",
      "column"
    ],
    "argumentDescriptions": {
      "line": "1-based source line.",
      "column": "1-based source column."
    }
  },
  "diagnostic.reason.toml_duplicate_unlocated": {
    "template": "Duplicate TOML key or table; source location unavailable and values omitted.",
    "args": [],
    "argumentDescriptions": {}
  },
  "diagnostic.reason.toml_syntax": {
    "template": "Invalid TOML syntax at line {line}, column {column}; source values omitted.",
    "args": [
      "line",
      "column"
    ],
    "argumentDescriptions": {
      "line": "1-based source line.",
      "column": "1-based source column."
    }
  },
  "diagnostic.reason.toml_syntax_unlocated": {
    "template": "Invalid TOML syntax; source location unavailable and values omitted.",
    "args": [],
    "argumentDescriptions": {}
  }
};
