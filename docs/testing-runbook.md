# Phantom Testing Runbook

Use this runbook whenever the user asks to "test", "驗證", "測一下", "deploy
看", or "確認沒 regression". Do not claim a change is "tested" unless the
relevant sections below have been run, or explicitly say which sections were
not run.

## 0. Pick The Scope

Start by stating the test scope:

- `local`: compile/unit/static checks only.
- `remote-smoke`: local checks plus deploy/run smoke on test hosts.
- `manual-ready`: remote-smoke plus links and a checklist for the user to
  exercise real browser/native interactions.
- `release`: fresh install or reinstall path using release artifacts or the
  installer override path, then manual-ready checks.

If the change touches capture, encoder, transport, Windows service mode,
display topology, WebRTC, input, cursor, audio, installer, or web client, use at
least `remote-smoke`.

## 1. Local Preflight

Always run:

```bash
git status --short
cargo fmt --all -- --check
git diff --check
cargo check -p phantom-server --features webrtc
cargo test -p phantom-server --lib
```

If code under `crates/web/` changed, run WASM before building/deploying the
server:

```bash
wasm-pack build crates/web --target web --no-typescript
cargo check -p phantom-server --features webrtc
```

If protocol/core/client code changed, add the relevant broader checks:

```bash
cargo test --workspace
cargo check -p phantom-client
```

If warnings appear in a release build, fix them before deploying unless there
is a clearly documented reason not to.

## 2. Windows Service And Display Smoke

Run this for any Windows service, DXGI, VDD, GDI, input, cursor, WebRTC/WSS, or
installer change.

Build a Windows release binary on a Windows/MSVC host or CI-equivalent:

```powershell
cargo build --release -p phantom-server --features audio
Get-Item target\release\phantom-server.exe | Select-Object FullName,Length
(Get-FileHash -Algorithm SHA256 target\release\phantom-server.exe).Hash
```

Deploy to one Windows canary first. Do not deploy to both Windows hosts until
the canary smoke is clean.

Service smoke:

```powershell
sc.exe stop PhantomServer
Start-Sleep -Seconds 3
Get-Process phantom-server -ErrorAction SilentlyContinue | Stop-Process -Force
Copy-Item <new-phantom-server.exe> "C:\Program Files\Phantom\phantom-server.exe" -Force
sc.exe start PhantomServer
Start-Sleep -Seconds 5
sc.exe query PhantomServer
(Get-FileHash -Algorithm SHA256 "C:\Program Files\Phantom\phantom-server.exe").Hash
```

From the local machine:

```bash
curl -k -I --max-time 10 https://<host>:9901/
```

Log smoke must confirm:

- Service is `RUNNING`.
- Browser endpoint returns HTTP 200.
- Agent IPC connects.
- Candidate logs show generation-scoped IPC, a non-empty capture keyframe, and
  `Committed agent generation ...` before the previous generation is retired.
- A standard non-administrator user's Default-desktop agent connects on its
  first launch; there is no 10-second IPC timeout/relaunch loop.
- Display manager bootstraps on the expected desktop.
- Windows Default desktop reaches `Tier 1 DXGI/NVENC ready 1920x1080` when GPU/VDD is expected.
- Winlogon/login uses GDI with the secure desktop's primary-screen metrics,
  preserving Windows-owned topology even if CCD reports a different size.
  Phantom VDD provisioning waits until Default is active; an external display
  manager keeps ownership of its topology.
- Tier 1 startup does not emit a rapid `DXGI capture reset OK` loop or fall back
  merely because the shell initially produced a black transition frame.
- No repeated black-frame, tiny-keyframe, reconnect, or fallback loop continues after startup.

For Windows display-manager changes, also verify these user-visible cases
before calling it good:

- Open File Explorer from the taskbar or Start menu. It must appear on the
  streamed display, not on a hidden physical/secondary display.
- Drag a window continuously. Click, drag, and typing must remain responsive.
- Lock -> unlock: viewer should stay connected or recover without refresh, and
  should transition from login screen to Default desktop.
- On Winlogon, focus the password field and enter the real test password.
  Characters must appear and submission must reach Default; an agent-side
  `received input` log alone does not count as keyboard success.
- Sign out -> sign in: no permanent black screen; no duplicate/stacked display
  layout; the login screen must fill its advertised frame instead of appearing
  as an 800x600 surface in one corner of a 1920x1080 frame. A temporary
  Winlogon-to-Default resolution change is valid when Windows creates a new
  secure-desktop console at a different native size.
- During sign out, any old-session candidate is cancelled promptly; the service
  must not spend the full 10-second IPC/capture timeout on a stale generation.
- Compare CCD topology before and after lock/unlock/sign-out. Runtime recovery
  must not enable a second display, change primary, or relocate the active path.
- If both WSS and RTC are in scope, test both `https://<host>:9901/?wss` and
  `https://<host>:9901/?rtc`.

## 3. Linux GPU Smoke

Run this for Linux capture, installer/autologin, NVFBC/NVENC, audio, transport,
or shared server changes.

Host smoke:

```bash
ssh <host> 'systemctl --user status phantom-server --no-pager || true'
ssh <host> 'pgrep -a phantom-server || true'
curl -k -I --max-time 10 https://<host>:9901/
```

Log smoke must confirm:

- `DISPLAY` and `XAUTHORITY` are correct for the running session.
- GPU hosts expected to use NVFBC/NVENC log `NVFBC` and `NVENC`, not accidental
  scrap/GDI-like fallback.
- First keyframe is non-black and roughly the expected resolution.
- No fast-forward/backlog, reconnect, or audio underrun loop continues after startup.

Manual Linux checks:

- Open web client and verify non-black desktop.
- Move/drag a window for at least 10 seconds.
- With a synthetic test desktop, change to another supported resolution and
  back while the viewer remains connected, then repeat with a mode change
  between connections. Test both RTC and WSS. Check decoded dimensions,
  non-corrupt colors, advancing frames, and actual pointer coordinates near
  the far edge of the display. A correct picture alone does not prove input
  mapping follows the new resolution. Restore the original display mode.
- If lock/suspend/autologin code changed, lock or sign out and verify recovery
  according to the install mode.

## 4. WebRTC / WSS Transport Checks

Run when transport, queueing, decoder, WebCodecs, audio, cursor, or browser
code changes.

Browser checks:

- WSS: `https://<host>:9901/?wss`
- RTC: `https://<host>:9901/?rtc`
- Verify first frame, sustained frame updates, click/drag, keyboard, cursor
  shape/position, and audio if audio changed.
- Leave RTC on a static desktop for at least 15 seconds, then drag a window.
  The displayed frame must follow the drag promptly; rising decoded-frame
  counts alone do not prove presentation is current. Include audio playback
  when changing RTCP sender-report clocks.
- Switch tabs away and back; verify no stale backlog fast-forward.
- Disconnect/reload repeatedly; old sessions should close and new sessions
  should receive a fresh keyframe.
- Hard-close the active RTC browser without opening a replacement. The server
  must log the RTC client disconnect and stop that session's `rtc-stats` within
  35 seconds, then accept a fresh RTC connection normally.
- Inspect browser stderr/`webrtc-internals`; there must be no repeating
  `Failed to unprotect RTCP` or SRTP/SRTCP authentication errors.

If measuring performance, record both user-visible notes and machine stats:

- Browser decoded frames / dropped frames / RTT where available.
- Server log FPS, encode ms, bitrate, RTT/jitter, audio underruns.
- Whether the test was WSS or RTC.

### Sustained desktop audiovisual soak

For media/lifecycle acceptance, open
`scripts/validation/media-soak-source.html` in the selected canary's desktop
browser at 1920x1080. Enable autoplay for that isolated test browser. The fixture
animates a UTC millisecond barcode and plays a continuous 440 Hz tone.
Use a separate Chrome profile for the viewer, with a CDP port and autoplay;
muting its local output does not mute the captured guest signal.

Run each transport separately, using a different empty output directory:

```bash
node scripts/validation/run-media-soak.mjs rtc <host>:9901 1800 9271 /tmp/phantom-rtc-soak
node scripts/validation/run-media-soak.mjs wss <host>:9901 1800 9271 /tmp/phantom-wss-soak
```

The runner excludes ten seconds of startup, then measures the full requested
interval. It saves sampled pixel ages, PCM bins, native RTC statistics, browser
console output and endpoint screenshots. Major persistent failures stop early
and remain failed results. The exact favicon 404 is retained as an ignored
network error. Also collect Windows process/GPU samples using
`scripts/validation/sample-windows-soak.ps1`, and inspect browser stderr plus
server logs. The runner's JSON alone does not evaluate resource growth or
classify native SRTP errors. Record the source/receiver clock assumptions and
keep synthetic PCM measurements distinct from subjective listening.

For an unexplained WSS silence interval, repeat in a separate output directory
with `PHANTOM_SOAK_AUDIO_DIAGNOSTICS=1`. This adds encoded arrival timing,
decoded PCM amplitude and SAB-worklet underflow events to the samples. It does
not change prefill, output samples or pass/fail gates. The initial record
includes startup, so keep its underflows separate from the measured interval.
Client-assigned WebCodecs timestamps cannot prove wire sequence continuity;
arrival gaps alone do not locate the delay to the server, network or browser.

Complete interactive guest diagnostics before the measured interval. Even a
hidden task that launches a console executable can disturb the captured desktop;
one such launch coincided with an invalid barcode sample in the Win11 soak.
During measurement, keep the source, viewer, display and audio settings fixed.
If another test contaminates the interval, retain that failed attempt and run
a fresh full interval rather than excluding its affected samples.

With the local `transport_smoke` fixture and an independent Chrome instance,
exercise WSS audio cleanup under delayed module completion and without SAB:

```bash
node scripts/validation/test-audio-reconnect.mjs 9272 9921 /tmp/phantom-audio-reconnect
```

This forces three reconnects per path, requires every old AudioContext to close
and a single active context to deliver tone, injects focus while each WSS
connection is still CONNECTING, requires a video keyframe, and rejects browser
exceptions. Each new fixture connection also requires its distinct cursor state
and bitmap to be applied, so a cached cursor cannot hide a missing subscription.
Calibrate the independent PCM probe with:

```bash
node scripts/validation/test-media-soak-monitor.mjs
```

When changing SAB playout buffering, replay the retained packet-arrival trace
through the actual embedded worklet, including the LAN and long-outage controls:

```bash
node scripts/validation/test-wss-audio-buffer.mjs
```

The trace uses relative arrival times and frame counts with nonzero marker
samples. It tests buffering behavior; live PCM/tone acceptance remains required.

For SAB producer changes, also use the local fixture and isolated browser:

```bash
node scripts/validation/test-wss-audio-ring.mjs 9272 9921 /tmp/phantom-audio-ring
```

This suspends the consumer and sends marked native AudioData into the actual
WASM output callback. It requires full/partial queues and oversized packets to
preserve unplayed samples, correct reuse across ring wrap, and closed AudioData
for both accepted and dropped packets. It does not mask live delivery outages.

The dominant-tone estimate tolerates extra zero crossings from codec artifacts;
the RMS and continuous-silence gates remain unchanged. Record client audio-device
changes if a receiver output stall coincides with active inbound RTP. A failed
soak remains failed even when a receiver-device transition explains the gap.
It supplements the real desktop reconnect, lock/unlock and hard-close checks.

For cursor changes, also exercise in-place reconnect for both transports with
the same local fixture and isolated browser:

```bash
node scripts/validation/test-cursor-reconnect.mjs 9272 9921 /tmp/phantom-cursor-reconnect
```

Every new session must apply its distinct cursor id after subscribing to both
state and shape updates. Verify arrow/I-beam changes on a real Windows textbox
after repeated WSS socket and RTC peer closure; page reload resets all browser
state and can hide a missing subscription in either transport's cleanup path.

### Local synthetic presentation smoke

For RTP/RTCP timing changes, this fixture exercises the embedded browser and
real media transport without capturing a desktop or injecting OS input. It
plays three seconds of motion, holds the exact pixels for twenty seconds
(including requested keyframes), then resumes motion for twelve seconds.
A quiet 440 Hz Opus tone continues through the idle interval.

```bash
cargo run -p phantom-server --example transport_smoke
```

Run a separate Chrome instance with a temporary profile, remote debugging on
port 9270, `--ignore-certificate-errors`, and
`--autoplay-policy=no-user-gesture-required`. Then, with Node 22 or newer:

```bash
node crates/server/examples/probe_transport.mjs rtc
node crates/server/examples/probe_transport.mjs wss
```

Each probe opens and closes its own tab. The JSON result decodes a timestamp
from the rendered pixels, verifies at least fifteen seconds of unchanged
pixels, and requires at least six seconds of resumed motion with every sampled
frame less than one second old. It also records receiver video/audio counters.
`--no-audio` on the fixture provides a video-only comparison. Optional probe
arguments select the Chrome debugging port and fixture HTTPS port. Keep the
default idle duration when using the probe's fixed 38-second sampling window.

The fixture advertises loopback ICE and serves synthetic media over HTTPS on
port 9921 (the listener binds all interfaces), with UDP on 9923. Stop it after
testing. This local check supplements remote smoke; it does not verify
GPU capture, Windows desktop transitions, WAN behavior, or audible playback.

### Windows audio capture isolation

When the browser reports audio concealment or underruns despite zero network
loss, compare the device stream before changing transport buffering:

```powershell
cargo run --release -p phantom-server --example windows_audio_buffer_probe
```

Play a known continuous tone in the interactive Windows session while this
45-second diagnostic runs. It compares 20 ms / 100 ms polling capture and
100 ms event-driven capture. It reads packet metadata only. A healthy active
48 kHz source produces about 240,000 frames per five seconds. Compare
`missing_frames` and `discontinuities` across modes; exclude startup and the
intentional start/stop of playback. Repeat with a second playback source before
attributing gaps to the device path. Nonzero decoded audio or a successful
capture start is insufficient for an audio-quality pass.

Use `--tone` for an independent, quiet 440 Hz WASAPI renderer with a 200 ms
buffer. Exclude the first interval's prefill. `empty_polls=0` and substantial
`min_padding` show that playback did not run out of queued samples. Compare
`clock_delta / clock_frequency` with `elapsed_ms / 1000`: a slow device clock
with a full render queue distinguishes endpoint timing from producer starvation.
This option plays a test tone; the capture probes still inspect metadata only.

An optional `--timer-1ms` comparison calls `timeBeginPeriod(1)` for the diagnostic
process and pairs it with `timeEndPeriod(1)` on exit. Verify
`timer_period_released=true`. It does not change a driver or persistent setting,
and Windows does not guarantee that it changes other processes' timer behavior;
see [timeBeginPeriod](https://learn.microsoft.com/en-us/windows/win32/api/timeapi/nf-timeapi-timebeginperiod).

Event-driven loopback is supported on Windows 10 1703 and newer; see
[Microsoft's loopback documentation](https://learn.microsoft.com/en-us/windows/win32/coreaudio/loopback-recording).
Packet position and discontinuity semantics are defined by
[IAudioCaptureClient::GetBuffer](https://learn.microsoft.com/en-us/windows/win32/api/audioclient/nf-audioclient-iaudiocaptureclient-getbuffer).

For the diagnosed AWS Virtual Speakers 7.1 event-mode failure, an administrator
can use `scripts/windows-audio-polling.ps1` with the **verified render endpoint
GUID**. This is an explicit machine repair, not an installer or server default:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\scripts\windows-audio-polling.ps1 -EndpointId '<render-endpoint-guid>' -WhatIf
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\scripts\windows-audio-polling.ps1 -EndpointId '<render-endpoint-guid>'
# Restore the original setting from the per-endpoint backup:
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\scripts\windows-audio-polling.ps1 -EndpointId '<render-endpoint-guid>' -Restore
```

The script checks the AWS endpoint identity, backs up the original mode under
`%ProgramData%\Phantom\audio`, and restarts Windows Audio. Existing audio streams
are interrupted; reconnect the browser after the change. It does not change
registry ACLs, replace a driver, or reboot the VM. Microsoft's
[endpoint-property documentation](https://learn.microsoft.com/en-us/windows/win32/coreaudio/audio-endpoint-properties)
reserves these properties for the audio service/OEM, so do not apply this
compatibility override automatically or to unrelated devices. Driver updates
may recreate the endpoint or reset its properties; remeasure before reapplying.
The example's execution-policy override applies only to that PowerShell process.

Validate both parts of the repair: approximately 240,000 frames per five
seconds with zero steady-state gaps using the 100 ms probes, then RTC/WSS
playback continuity. A 20 ms polling buffer can overflow even after the device
clock is repaired; Phantom uses 100 ms capture capacity and drains available
packets immediately, still encoding 20 ms Opus frames. If the endpoint change
does not improve the independent probe, restore its backup.

## 5. Native Client Checks

Run when core protocol, cursor, input, QUIC/TCP, decoder, or client UI changes.

```bash
cargo run --release -p phantom-client -- --no-encrypt -c <host>:9900
```

For an isolated native renderer check, start the synthetic fixture with
`cargo run -p phantom-server --example transport_smoke -- --tcp --port 9920`
and connect the native client to `127.0.0.1:9920`. This fixture binds loopback,
ignores clipboard payloads and never injects input into the host OS.
It supplements real remote input/cursor checks; decoded frames do not replace
visual verification of the native UI.

Check:

- First frame appears.
- Mouse move/click/drag and keyboard work.
- Cursor shape and hotspot are aligned.
- Top bar/UI changes render correctly.
- Audio works if audio changed.

## 6. Reporting Rules

When reporting results, include:

- Commit/worktree identity if relevant: branch, dirty status, binary hash, size.
- Which runbook sections ran.
- Which hosts were touched.
- URLs for manual validation.
- Exact failures or skipped sections.

Use precise wording:

- `passed local checks` means only section 1 passed.
- `passed remote smoke` means local checks plus service/log/HTTP smoke passed.
- `manual-ready` means the user still needs to exercise real interactions using
  the supplied links.
- Never say `all good` if lock/login/logout/window placement was not manually
  or explicitly tested for Windows display-manager changes.
