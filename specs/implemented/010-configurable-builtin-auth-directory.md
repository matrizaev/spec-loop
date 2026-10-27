# Spec 010: Configurable Built-in Authentication Directory

**Status:** Complete

## Goal

Allow users to choose where the built-in harness stores its `auth.json` registry, while preserving current default paths and credential protections when no override is set.

## Existing behavior

- Built-in authentication stores its registry in `auth.json` under platform-specific user configuration directories.
- Linux and other non-macOS, non-Windows platforms use `$XDG_CONFIG_HOME/spec-loop/auth.json`, or `$HOME/.config/spec-loop/auth.json` when `XDG_CONFIG_HOME` is unset.
- macOS uses `~/Library/Application Support/Spec Loop/auth.json`; Windows uses `%APPDATA%\Spec Loop\auth.json`.
- `spec-loop auth` commands do not load a project profile. Built-in workflow token lookup and refresh use the same registry path resolver.
- The registry may contain file-backed credentials. Existing directory/file permission restrictions and project-boundary checks protect that data where supported.

## Requirements

### User-selected directory

- Add environment variable `SPEC_LOOP_AUTH_DIR` as an optional override for the built-in authentication directory. Its value is the directory that directly contains `auth.json`; do not append another `spec-loop` or `Spec Loop` component.
- Apply the override on every supported platform and give it precedence over platform defaults and `XDG_CONFIG_HOME`.
- Use the same resolved registry path for `auth login`, `auth status`, `auth logout`, built-in workflow token lookup, and token refresh/persistence. The authentication commands must remain profile-independent.
- Do not change Pi configuration or authentication behavior. Do not add the override to repository profile schema.
- When `SPEC_LOOP_AUTH_DIR` is unset, preserve current platform defaults and Linux `XDG_CONFIG_HOME` behavior exactly. Unsetting the override must make existing default-location credentials available as before.
- Treat an empty or relative override as invalid. Report an actionable configuration error; do not silently ignore it or fall back to a default. Resolve and validate the target before creating directories or writing credentials.
- Preserve existing requirements that authentication storage remain outside the active project and that directories/files receive existing owner-only restrictions where supported. An override does not permit storing credentials in the worktree. Do not weaken existing path validation or permission handling.
- Do not migrate, copy, or delete existing registry or credential-store data when the override changes. Do not silently fall back to another location if the selected override cannot be used.

### Documentation and security

- Document `SPEC_LOOP_AUTH_DIR`, its direct-parent semantics, precedence, and unset defaults in README and applicable authentication help.
- Explain that `auth.json` may contain plaintext credentials when the operating-system credential store is unavailable; custom location does not encrypt credentials or isolate them from same-user processes.
- Keep existing warnings about credentials staying outside the worktree, owner-only file protections, and the built-in shell's same-user unsandboxed access. Do not describe the selected directory as a credential sandbox.

## Acceptance criteria

1. With `SPEC_LOOP_AUTH_DIR` set to an absolute directory outside the project, built-in authentication resolves `auth.json` directly beneath that directory on all supported platforms, independent of `XDG_CONFIG_HOME`.
2. Login, status, logout, workflow credential lookup, and refresh/persistence all use the same selected registry. Deterministic tests exercise resolution and file-backed operations without network access or live credentials.
3. With the variable unset, tests confirm existing platform defaults and Linux `XDG_CONFIG_HOME` precedence remain unchanged.
4. Empty and relative overrides fail with actionable errors before creating directories or files. Overrides resolving inside the active project are rejected before credential access or mutation.
5. Custom-location file-backed credentials retain existing owner-only directory/file permissions where supported. Invalid paths and filesystem failures do not trigger silent fallback to the default location.
6. Pi behavior and repository profile parsing remain unchanged. README and authentication help document the override and security boundary accurately.
7. Run `make check` after implementation. Tests require no network, live model, credentials, or developer worktree state.

## Non-goals

- Changing built-in authentication protocols, credential-store selection, file format, keyring identifiers, or provider behavior.
- Migrating existing credentials, supporting multiple authentication directories in one invocation, or changing Pi's resource configuration.
- Allowing credentials inside a project, encrypting the file fallback, or sandboxing built-in shell commands.
