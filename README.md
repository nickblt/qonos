# Qonos

**Work in Progress**

Qonos is a Rust service that bridges Qobuz Connect to Sonos speakers. It allows your Sonos system to appear as a Qobuz Connect target, so you can control playback from the Qobuz app while audio plays through your Sonos speakers.

## How It Works

```
Qobuz App (phone/desktop)
    │
    ▼ WebSocket (QConnect protocol)
Qonos
    │
    ▼ WebSocket (Sonos API)
Sonos Speaker ──► Qobuz API (Sonos's native connection)
                      │
                      ▼
                  Audio Stream
```

Qonos doesn't stream audio itself. Instead, it translates commands from Qobuz Connect and forwards track IDs to Sonos, which plays them using its built-in Qobuz integration.

## Dependencies

This project leverages two companion libraries:

- **[qonductor](https://github.com/nickblt/qonductor)** - Handles the Qobuz Connect protocol (mDNS discovery, device registration, protobuf messaging)
- **[sonos-websocket](https://github.com/nickblt/sonos-websocket)** - Controls Sonos speakers via their WebSocket API

## Requirements

- Rust 2024 edition
- Sonos speakers with Qobuz configured as a music service
- Qobuz account and app credentials

## Usage

```bash
# Set your Qobuz app ID
export QOBUZ_APP_ID="your_app_id"

# Run with logging
RUST_LOG=qonos=info,qonductor=info cargo run
```

Qonos will discover Sonos speakers on your network and register them as Qobuz Connect renderers. They should then appear in the Qobuz app's device picker.

## Status

This is an early work in progress. Current functionality:

- [x] Sonos speaker discovery
- [x] Qobuz Connect device registration
- [x] Basic playback commands (play/pause/stop)
- [ ] Queue synchronization
- [ ] Volume control
- [ ] Seek support
- [ ] Multi-room/group handling

## License

MIT
