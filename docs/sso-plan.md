# SSO-to-desktop plan

Goal: user logs into our CSP → lands in VM desktop. No second password entry, no OS-level auth UI.

## Architecture

```
┌───────────────────────────────────────────────┐
│  LogonUI.exe (Windows) / GDM (Linux)          │
│    ├── phantom_cp.dll                         │  ← new artifact #1
│    └── libpam_phantom.so                      │  ← new artifact #2
│          ↕ named pipe / unix socket            │
│  phantom-server.exe / phantom-server (SYSTEM) │  ← existing agent
│    ↕ WSS + JWT                                 │
│  Our CSP (IdP)                                │
└───────────────────────────────────────────────┘
```

phantom-server is already the agent — has JWT verification, Session-0 IPC, `CreateProcessAsUser` plumbing. We add:

1. **`crates/cred-provider-win`** → `phantom_cp.dll` (COM, loaded by LogonUI)
2. **`crates/pam-phantom`** → `libpam_phantom.so` (PAM module, loaded by GDM)
3. **New IPC protocol** between phantom-server and these two, carrying `{sub, vm_id, session_ticket}`

Nothing else in phantom-server changes — it already has all pieces.

## Credential strategy

**No persistent OS password stored anywhere.**

- Linux: phantom-server writes short-lived ticket (session UUID) + username to `/run/phantom/auth`; PAM module reads on login; bypasses `pam_unix`. Account must exist locally.
- Windows: phantom-server pushes `{user}` over named pipe to CP; CP calls LSA with `MSV1_0_S4U_LOGON` (NT S4U — local account, no password needed; caller must hold `SeTcbPrivilege`, which LogonUI does).
- No `cred.dat`, no DPAPI, no rotation, no lazy sync. OS never requires a password in normal flow.

Password-change bypass problem dissolves because the OS-local password is no longer part of the auth path.

## Phases

### Phase MVP (this session) — prove the PAM injection point works

- `crates/pam-phantom` cdylib; `pam_sm_authenticate` reads `/run/phantom/auth`, matches username, returns `PAM_SUCCESS`
- Install to `/lib/x86_64-linux-gnu/security/pam_phantom.so`
- Test via `/etc/pam.d/su` first (no GUI needed): `su - horde` from another user with no pw prompt
- Success = PAM stack mechanism is wired

### Phase 1 — Wire PAM to GDM + phantom-server

- Move config to `/etc/pam.d/gdm-password`, `auth sufficient pam_phantom.so` on top
- phantom-server writes `/run/phantom/auth` after JWT `sub` verified
- Test: boot VM (no autologin), phantom connects → GDM click user → logged in
- Remove `install.sh --autologin` once this works

### Phase 2 — Windows Credential Provider

- `crates/cred-provider-win` → `phantom_cp.dll`
- Use MS `SampleCredentialProvider V2` as skeleton; re-implement in Rust with `windows` crate
- Named pipe `\\.\pipe\phantom-cp` with SDDL `D:(A;;GA;;;SY)(A;;GA;;;BA)`
- CP on `CPUS_LOGON` + `CPUS_UNLOCK_WORKSTATION`: polls pipe → `MSV1_0_S4U_LOGON` → return serialization
- Codesigning: dev uses test-signing mode (`bcdedit /set testsigning on`); prod needs cert

### Phase 3 — Production polish

- CP error UX (LSA denies, account missing, pipe down → fall through to default CP)
- Audit logging, session rotation on JWT exp
- Ctrl+Alt+Del change-password interception (if needed — probably not, since pw is unused)
- Codesigning pipeline in CI

## Verification gates — results (researched)

1. **`MSV1_0_S4U_LOGON`** is correct API, BUT returned token is degraded: no DPAPI access, no keyring unlock, no network creds, no saved credential-manager entries. OK for local browser/files; user-visible side-effect: GNOME Keyring / Chrome saved passwords / SSH keys in keyring all need manual unlock. Acceptable trade-off; document as known limitation. `KERB_S4U` rejected — requires caller to be domain account.

2. **Rust `windows` crate** has `ICredentialProvider` + `ICredentialProviderCredential` under `windows::Win32::UI::Shell` (features `Win32_UI_Shell` + `Win32_Security`). Only public Rust CP implementation is a 7-commit learning sample — we'd be trail-blazing. Fallback: MS's C++ sample (`Windows-classic-samples/Samples/CredentialProvider`) if Rust COM gets too painful.

3. **PAM crate: `pamsm`** — has `pam_module!` macro emitting cdylib exports, production user is Authentik. Use this.

4. **GDM Ubuntu 24** uses `/etc/pam.d/gdm-password`. Insert **after** `pam_nologin` + root-block `pam_succeed_if`, **before** `@include common-auth`. Line 1 is wrong (bypasses shutdown lock). `sufficient` short-circuits on `PAM_SUCCESS`, works as expected.

5. **CP codesigning — non-issue for dev.** CP is user-mode (loaded by LogonUI.exe); unsigned DLL loads fine once CLSID is registered. `bcdedit /set testsigning` applies only to kernel-mode drivers, not CPs. Prod fleets with WDAC/Secure Boot enforcement eventually need Authenticode, but MVP / CI / test fleet do not.

## MVP scope (this work)

Just prove the PAM injection point. No phantom-server integration yet.

- `crates/pam-phantom` standalone cdylib, `pamsm` dep
- `pam_sm_authenticate` reads `/run/phantom/auth` (contents = username), compares to `PAM_USER`, returns `PAM_SUCCESS` on match
- Install to `/lib/x86_64-linux-gnu/security/pam_phantom.so`
- Test via `/etc/pam.d/su` (CLI only, no GUI risk): `su - horde` from another user succeeds without password prompt
- If green → layer on phantom-server writing `/run/phantom/auth` and move config to `/etc/pam.d/gdm-password`
