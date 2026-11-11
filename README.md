# Wireless Display for Linux

Experimental Linux virtual display host written in Rust. The target platform is
KDE Plasma on Wayland; Android TV streaming is a future milestone.

> **Status: WD-005.2 diagnostic prototype, not production-ready.**
> Neither TV streaming nor hardware video encoding is implemented.

## Current implementation

- `src/portal.rs`: XDG Desktop Portal ScreenCast lifecycle, D-Bus calls,
  response validation and PipeWire file descriptor acquisition.
- `src/capture.rs`: GStreamer pipeline, PipeWire source, bus error reporting
  and captured frame statistics.
- `src/main.rs`: application state transitions, signal subscriptions and
  non-blocking orchestration of the capture startup.

The application requests source type `4` (virtual monitor), connects to the
PipeWire node returned by the portal, waits up to 45 seconds for the first
frame and measures frames received over the following 15 seconds.

## Dependencies (Manjaro/Arch)

```sh
sudo pacman -S --needed rust cargo pkgconf glib2 gstreamer gst-plugins-base gst-plugins-good
```

You also need `pipewire`, `xdg-desktop-portal`, and a backend with support
for virtual ScreenCast sources, such as `xdg-desktop-portal-kde`.

## Development

```sh
cargo fmt --all -- --check
cargo check
cargo test
cargo run
```

For PipeWire negotiation diagnostics:

```sh
GST_DEBUG="2,pipewire*:6" cargo run
```

Do not claim working video capture until actual frames have been reported.
This revision has not yet been validated on the target machine.

## Commit and release convention

Allowed commit prefixes: `feat`, `fix`, `docs`, `style`, `refactor`, `test`,
`chore`.

Versions use `MAJOR.MINOR.PATCH`. Pre-release suffixes are `-alpha`, `-beta`,
`-rc` (for example `1.0.0-rc.1`). Stable tags require a verified release.

## Roadmap

- WD-005: stable KDE virtual monitor and PipeWire video capture.
- WD-017: H.264 hardware encoding.
- WD-018: network transport and latency measurement.
- WD-019: Android TV receiver.
- Packaging and release automation, including AUR.

## License

GNU GPL version 3 only (`GPL-3.0-only`). See `LICENSE`.
