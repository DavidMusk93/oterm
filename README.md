# oterm — Online Terminal

A state-of-the-art, single-binary Rust web terminal with **server-bound TOTP authentication**, **5-minute OTP session keepalive without disrupting running PTYs**, **resumable monotonic-sequence binary WebSocket streaming**, and desktop + mobile terminal ergonomics.

## Architecture

```text
[ Browser: xterm.js v5 + WebGL + Unicode11 + Fit + Search + Mobile Bar ]
         |
         |  HTTPS / WSS  (Binary Multiplexed Protocol v1 + 20s Keepalive)
         v
[ oterm (Single Rust Binary: Axum + Tokio + Embedded Assets) ]
  |-- AuthManager (Server-side RFC 6238 TOTP, 5-min TTL, Replay & Rate Guard)
  |-- SessionManager (Multi-tab Persistent PTYs via portable-pty)
  `-- SeqRingBuffer (4 MiB/session monotonic byte offset cache for delta resume)
```

## Key Features

- **Server-Side OTP Binding Only**: TOTP secret is generated and stored exclusively on the server (`0600`) via `oterm otp init`. There is no web enrollment endpoint.
- **5-Minute Auth Keepalive, Zero PTY Disruption**: After 5 minutes (`--auth-ttl 300`), the server sends an `AUTH_LOCK` frame and closes the WebSocket (`4401`). The browser displays a glassmorphism OTP unlock overlay **without clearing the terminal canvas**, while the server keeps all PTYs alive and buffers output into the 4 MiB ring buffer. Re-entering OTP resumes seamlessly from the exact `last_seq` byte offset.
- **Binary Multiplexed Wire Protocol**:
  - `0x01 INPUT` / `0x02 RESIZE` / `0x03 ACK` / `0x04 PING` / `0x05 ATTACH`
  - `0x11 OUTPUT` / `0x12 SNAPSHOT` / `0x13 PONG` / `0x14 SESSION` / `0x15 AUTH_LOCK` / `0x16 ERROR`
- **Application-Layer Flow Control**: Client sends `ACK(seq)` inside `xterm.write()` completion callbacks; server enforces a 256 KiB in-flight window so high-throughput output (`cat huge.log`, AI CLIs) never freezes the browser.
- **Keepalive & Network Roaming**: 20-second `PING`/`PONG` heartbeat (with RTT display and Cloudflare 100s WS idle timeout protection), exponential backoff reconnect, and instant wakeup on `online` / `visibilitychange`.
- **SOTA Terminal UX**:
  - WebGL GPU-accelerated rendering with automatic DOM/Canvas fallback on context loss.
  - Multi-session tab bar (create, switch, double-click rename, kill).
  - Scrollback search (`Ctrl+Shift+F`, regex + case-sensitive), smart copy/paste (`Ctrl+C` copies when text is selected, bracketed paste on `Ctrl+Shift+V`), font zoom (`Ctrl+`/`Ctrl-`/`Ctrl+0`), 5 themes.
  - Mobile `VirtualKeyboard` / `visualViewport` handling + sticky `Ctrl`/`Alt` accessory key bar.

## Build & Run

```bash
# 1. Build frontend assets (embedded into Rust binary via rust-embed)
cd web && npm install && npm run build && cd ..

# 2. Build release binary
cargo build --release

# 3. Initialize server-side TOTP secret (scan QR with Authenticator / 1Password)
./target/release/oterm otp init --secret-path /etc/oterm/totp.secret --account xyz69.top

# 4. Run server
./target/release/oterm serve --bind 127.0.0.1:8782 --base-path /oterm --auth-ttl 300
```

## CLI Reference

```bash
oterm otp init   [--secret-path /etc/oterm/totp.secret] [--issuer oterm] [--account xyz69.top] [--force]
oterm otp show   [--secret-path /etc/oterm/totp.secret]
oterm otp verify <6-digit-code> [--secret-path /etc/oterm/totp.secret]
oterm serve      [--bind 127.0.0.1:8782] [--base-path /oterm] [--auth-ttl 300] [--ring-buffer-bytes 4194304]
```

## License

MIT
