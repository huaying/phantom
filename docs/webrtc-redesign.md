# Phantom — WebRTC Redesign

Status: baseline implemented in v0.5.3 (media tracks for video/audio,
DataChannels for input/control). This doc is kept as design rationale and
follow-up checklist for remaining tuning.

## Problem statement

The current WebRTC path sends desktop video over a reliable, ordered
DataChannel and reassembles large frames with custom chunking. This works for
basic connectivity, but it is the wrong transport model for interactive remote
desktop:

- During window dragging / scrolling / large motion, the encoder generates a
  continuous stream of deltas where **old frames lose value quickly**.
- Reliable + ordered delivery preserves stale frames instead of letting the
  newest frame overtake them.
- Large keyframes and bursty frame production hit SCTP buffering limits, so we
  fall into chunk queues and `BufferedAmountLow` pacing.
- Queue-full handling currently drops video silently, which is survivable for a
  future lossy stream but not for H.264 P-frame dependency chains.

Observed effect:

- WebRTC often feels worse than WSS while dragging, despite similar average FPS.
- The tail latency is worse: higher RTT/jitter excursions, occasional ICE
  disconnects, and visible "stuck / catch-up / brief disconnect" behavior.

## Root causes in the current implementation

Current implementation:

- `video` DataChannel: reliable + ordered
- `input` DataChannel: ordered + `maxRetransmits = 2`
- `control` DataChannel: reliable
- signaling: POST `/rtc`
- media decode: browser `WebCodecs`
- audio: still a separate WebSocket, not WebRTC media

Problems:

1. **Video is carried as arbitrary data, not media**
   - We lose RTP pacing / media-oriented buffering / receiver feedback behavior.
   - We replace it with ad-hoc chunk queues.

2. **Large frame chunking is fundamental, not incidental**
   - `str0m` + SCTP buffering makes large writes fragile.
   - H.264 keyframes are large exactly when the stream is under stress.

3. **Reliable ordered delivery is a poor fit for stale desktop frames**
   - It optimizes correctness of old data, not usefulness of new data.
   - Remote desktop prefers "latest frame wins" once a frame is late.

4. **Backpressure policy is queue-based, not freshness-based**
   - We pause and drain.
   - We do not aggressively drop stale video and request clean recovery.

5. **Audio is not on the same transport model**
   - WebRTC video + WebSocket audio is an awkward hybrid.
   - It complicates latency reasoning and recovery behavior.

## New direction

Keep WSS as the default and stable browser transport.

Redesign WebRTC as:

- `video`: WebRTC media track
- `audio`: WebRTC media track
- `input`: DataChannel
- `control`: DataChannel

That means:

- WebRTC is no longer "DataChannel transport with custom video framing".
- WebRTC becomes a proper media transport for low-latency browser playback.
- DataChannels return to what they are good at: small control/data messages.

## Transport split

### Video track

Responsibilities:

- carry H.264 encoded frames as RTP video
- use WebRTC pacing / congestion handling instead of SCTP chunk queues
- let late/obsolete frames be superseded naturally by media behavior

Requirements:

- server-side packetization for H.264 Annex B / AVCC into RTP payloads
- browser receives normal video track
- browser rendering path can still use `<video>` or `MediaStreamTrackProcessor`
  depending on latency/processing needs

Questions to settle:

- whether to render directly with `<video>` first for simplicity
- whether we need `WebCodecs` integration later for custom paint / zero-copy-ish
  tuning

### Audio track

Responsibilities:

- move Opus audio to standard WebRTC audio track
- remove separate `/ws/audio` dependency for the WebRTC path

Benefits:

- audio/video share the same transport family
- fewer hybrid failure modes
- less duplicated reconnect logic

### Input DataChannel

Properties:

- ordered
- partial reliable (`maxRetransmits` low)

Use for:

- mouse move
- mouse buttons
- key press/release
- scroll

Rationale:

- input should arrive quickly
- old mouse positions are stale
- we do not want reliable backlog here

### Control DataChannel

Properties:

- reliable
- ordered

Use for:

- `ClientHello`
- resolution change
- clipboard sync
- paste text
- file-transfer control
- reconnect / diagnostics / feature negotiation
- explicit keyframe / resync requests

## Signaling and ICE

Minimum viable redesign:

- keep POST `/rtc` signaling at first
- add ICE servers support in browser config
- wait for ICE gathering to complete before POST if trickle is still absent
- then add trickle ICE later

Target phases:

1. host candidate + explicit gather-complete
2. STUN
3. TURN fallback
4. trickle ICE

Do **not** block the media-track redesign on full NAT traversal. The current
video-over-DC problem exists even on easy networks.

## Browser rendering strategy

Phase 1:

- receive video as normal WebRTC video track
- render with a hidden `<video>` element bound to the `MediaStream`
- paint the current frame into canvas only if we still need canvas-based input
  coordinate handling and overlays

Why:

- lowest implementation risk
- easiest way to validate whether the transport change alone fixes the
  drag/jitter issue

Phase 2 optional:

- revisit `WebCodecs` or `MediaStreamTrackProcessor` if we need tighter control
  over decode/display latency

## Recovery model

With media tracks, recovery should change too.

Current WSS logic:

- explicit `RequestKeyframe`
- optional `KeyframeFence`

New WebRTC logic:

- keep `RequestKeyframe` as control-plane message
- no ordered-fence concept across media + data; tracks and DCs are separate
- rely on fresh keyframe request and media decoder resync

That means:

- WSS and WebRTC recovery will intentionally differ
- the current WSS fence idea should **not** be copied onto media tracks

## What to delete or de-emphasize

If this redesign goes ahead, these should stop being central:

- `video` DataChannel transport
- custom chunked video framing for browser WebRTC
- SCTP backpressure queues for large video messages
- "WebRTC DataChannel is lower-jitter than media track" as a design principle

The current DataChannel-based WebRTC path can remain temporarily as:

- legacy experimental mode
- fallback for bring-up/testing

But it should not be the target architecture.

## Migration plan

### Phase 0: lock in current conclusions

- keep WSS as default
- mark current WebRTC DataChannel path experimental
- stop investing in deep video-over-DC tuning

### Phase 1: signaling + browser scaffolding

- introduce a new browser `?rtc2` or feature flag
- create `RTCPeerConnection` with:
  - one video transceiver / track
  - one audio transceiver / track
  - one input DC
  - one control DC

### Phase 2: server media output

- add RTP packetization path for H.264
- wire Opus audio into audio track
- keep existing session loop and encoded frame production if possible
- add a WebRTC media sender adapter rather than rewriting capture/encode first

### Phase 3: control-plane integration

- send `ClientHello` over control DC
- move resolution / clipboard / paste / reconnect control there
- keep auth on `/rtc`

### Phase 4: remove hybrid audio

- WebRTC path no longer opens `/ws/audio`

### Phase 5: test matrix

- drag windows
- scroll large pages
- multiple tabs
- tab background / foreground
- sleep / wake
- resize storms
- reconnect
- STUN / no-STUN / TURN

## Success criteria

The redesign is successful if WebRTC is measurably better on:

- drag smoothness
- long-motion stability
- disconnect frequency
- audio drop frequency
- subjective "stuck then catch up" behavior

Not just on average FPS.

## Decision

For Phantom:

- **WSS stays the primary browser transport**
- **WebRTC is redesigned around media tracks**
- **DataChannel is retained only for input/control**

This is the direction that best matches both WebRTC's intended model and the
empirical behavior we observed in the current implementation.
