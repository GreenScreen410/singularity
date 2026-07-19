# Singularity

A slowly drifting black hole that bends your entire Windows desktop like a real
gravitational lens - as a **single standalone app**. No terminal, no external
tools: run one executable and the hole wanders across your screen, warping
whatever it passes over.

Inspired by [ghostty-blackhole](https://github.com/s0xDk/ghostty-blackhole), but
freed from Ghostty and Claude Code.

![A black hole drifting across the desktop, lensing the windows around it](docs/demo.gif)

## Features

- **Real physics** - every pixel near the hole integrates its own null geodesic
  through the Schwarzschild metric (after Eric Bruneton's black-hole renderer,
  via [ghostty-blackhole](https://github.com/s0xDk/ghostty-blackhole)): a true
  shadow, an Einstein ring with mirrored secondary images, and a tilted
  Keplerian accretion disk whose far side arcs over and under the hole
- **Blackbody accretion disk** - Shakura-Sunyaev temperature profile,
  relativistic Doppler shift and beaming, gravitational time dilation
- **8 looks, switched live** - Inferno, Gargantua, Quasar, M87\* donut, Blazar,
  Face-on ember, Pure lens, Zen - pick from the tray menu, with a smooth
  crossfade (the original tuner's presets, exact values)
- **A real overlay** - fullscreen, always-on-top, click-through; your apps keep
  working underneath while the hole warps them. The overlay excludes itself
  from capture, so there is no mirror-feedback
- **Screensaver mode** - optionally appear only after N minutes without
  keyboard/mouse input, swelling out of nothing, and vanish on the first
  input (tray: Screensaver, or `idle_minutes` in the config file)
- **Update notice** - once a day the app asks GitHub whether a newer
  release exists and, if so, adds an "Update available" entry to the tray
  menu that opens the releases page. That is the only network access it
  ever makes; set `check_updates = 0` in the config file to disable it
- **Single self-contained exe** - no runtime, no installer, ~8 MB

## Build & run

### Windows (primary platform, tested)

Requires [Rust](https://rustup.rs) with the MSVC toolchain (the default on
Windows) and Windows 10 2004+ / Windows 11.

```powershell
cargo run --release
```

The overlay covers the screen, the hole drifts on its own, and clicks pass
through to your apps. Switch the disk look from the tray icon (the ^ overflow
area) - 8 presets from Inferno to Zen. Quit via the tray menu or Esc.

Cross-compiling from WSL also works: `rustup target add x86_64-pc-windows-gnu`,
install `mingw-w64`, then `cargo build --release --target x86_64-pc-windows-gnu`.

### macOS (UNTESTED - help wanted)

The macOS port (ScreenCaptureKit capture + `NSWindowSharingNone` self-exclusion
+ menu-bar presets) is structurally complete and kept type-checked via
`cargo check --target aarch64-apple-darwin`, but has never run on real
hardware - there is no Mac in this project's dev loop.

```sh
cargo run --release   # on a Mac
```

On first launch, grant **Screen Recording** permission (System Settings →
Privacy & Security) and relaunch. If you try it, success or failure reports
are equally welcome as issues.

### Linux

Not yet - neither X11 nor Wayland offers a way to exclude a window from
capture, so a live overlay feeds back into itself. A wallpaper-warp mode is
the likely path; contributions welcome.

## No-build alternative (ShaderGlass)

If you just want the effect over your desktop today without building anything,
`shaderglass/` contains the same lens as a shader for
[ShaderGlass](https://github.com/mausimus/ShaderGlass): load
`shaderglass/singularity.slangp` via **Shader → Import custom** with
**Input → Desktop** and **Output → Mode → Glass**. This is the stop-gap; the
standalone app above is the real goal.

## How it works

The desktop is captured into a GPU texture and plays the role of the lensed
"sky". For each pixel near the hole, the fragment shader integrates a photon
geodesic in the Schwarzschild metric (leapfrog, adaptive step): rays under the
critical impact parameter fall in (the shadow), escaping rays are projected
back onto the sky plane (lensing, Einstein ring, mirrored images), and every
crossing of the tilted disk plane accumulates blackbody emission shifted by
the relativistic Doppler factor. Far from the hole an analytic weak-field
formula takes over with a seamless handoff. The centre follows a slow
Lissajous path so the hole drifts on its own.

Tuning: disk looks live in `src/main.rs` (`PRESETS`), size in `HOLE_RADIUS`,
drift and integration budget in `src/singularity.wgsl` (`DRIFT_*`, `N_STEPS`).

## Performance & battery

Capture is zero-copy on Windows: each desktop frame is GPU-copied straight
into a shared D3D12 texture that the shader samples, so frames never touch
the CPU (with an automatic CPU fallback for setups where sharing is not
available). Only pixels near the hole pay for geodesic integration, frames
are delivered only when the screen changes, and the tray offers an FPS cap.
In screensaver mode the renderer is fully idle until the hole appears. It is
still a continuous visual effect though, so expect some battery cost while
the hole is on screen.

## A note on Windows SmartScreen

Release binaries are not code-signed (certificates are priced for companies,
not desk toys). The first launch of a downloaded exe may show "Windows
protected your PC" - click **More info → Run anyway**, or build from source.

## Credits

- Concept inspired by [ghostty-blackhole](https://github.com/s0xDk/ghostty-blackhole) by s0xDk
- Stop-gap runs on [ShaderGlass](https://github.com/mausimus/ShaderGlass) by mausimus

## License

MIT - see [LICENSE](LICENSE).
