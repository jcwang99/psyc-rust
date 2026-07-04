# AList And Single-Writer Push Design

## Goal

Fix the two issues exposed by live AList testing without weakening default safety:

1. `alist+` remote specs must work against AList WebDAV endpoints that live under `/dav`.
2. Push to conservative single-writer backends must remain blocked by default, but become possible when the operator explicitly confirms the risk.

## Design

### AList remote normalization

- Keep accepting `alist+https://token@host/path`.
- If the parsed path already starts with `/dav`, preserve it exactly.
- Otherwise normalize the root to `/dav/<path>`.
- Leave `webdav+` parsing unchanged.

This keeps existing explicit `/dav/...` remotes working while making the shorter AList form match the real server layout we verified on `https://alist.991198.xyz/`.

### Explicit risky single-writer push

- Keep the current default push path unchanged and safe.
- Add an explicit confirmation path from CLI -> SDK -> sync layer.
- When confirmation is present, publish using `WriterMode::SingleWriter` on conservative lock/lease backends instead of rejecting them up front.
- Continue to reject truly read-only backends.

This uses the existing single-writer lease/intent machinery rather than inventing a new publish mode.

## Verification

- Unit tests for `RemoteSpec::parse` covering both implicit and explicit `/dav`.
- Sync tests proving conservative WebDAV push still fails by default and succeeds with explicit risk confirmation.
- CLI tests proving the new flag is required to push to conservative WebDAV backends.
