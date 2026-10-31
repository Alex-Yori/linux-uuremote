# Vendored webrtc-rs 0.10.1 (patched by uu-remote-linux fork)

fork patches adopted verbatim:
- peer_connection_internal.rs: ICE state callback read a stale DTLS-state slot (always New);
  now wired through a weak slot so SCTP starts on real DTLS state.
- sctp_transport/mod.rs + data_channel/mod.rs: advertise/receive 262144-byte messages
  (libwebrtc advertises 262144; upstream 64KB/u16 buffer breaks the channel).
- webrtc-srtp: ROC half-space rule fix (RFC 3711 App. A) — delayed NACK across seq wrap
  no longer desyncs ROC and freezes video.
- webrtc-dtls: post-handshake delivery/handshake_done race hardening.
