---
webrtc-sys: patch
libwebrtc: patch
livekit: patch
livekit-ffi: patch
---

Add a V2 packet-trailer publish timing event exposing the final RTP timestamp,
SSRC, and keyframe state while preserving the existing event layout. Add
fallible video-sender handler creation, allow raw video sources to opt out of
the initial metadata-free keepalive frame, and expose received data-channel
reliability and ordering so applications can enforce metadata delivery
contracts.
