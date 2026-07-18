# Singularity

A slowly drifting black hole that bends your entire Windows desktop like a real
gravitational lens — as a **single standalone app**. No terminal, no external
tools: run one executable and the hole wanders across your screen, warping
whatever it passes over.

Inspired by [ghostty-blackhole](https://github.com/s0xDk/ghostty-blackhole), but
freed from Ghostty and Claude Code.

<!-- ![demo](docs/demo.gif) — drop a screen recording here to show it off -->

## Status — built in stages

This is a native Rust + wgpu app, built up incrementally:

- [x] **Stage 1** — wgpu window renders the black-hole shader over a test pattern
- [ ] **Stage 2** — feed the live desktop (`Windows.Graphics.Capture`) as the lensed background
- [ ] **Stage 3** — transparent, click-through, topmost overlay + `WDA_EXCLUDEFROMCAPTURE` (exclude our own window from capture)
- [ ] **Stage 4** — optimize: GPU-shared capture texture, render only near the hole

Right now you can run Stage 1 to see the black hole drift over a checker/gradient
background.

## Build & run

### Windows (primary platform, tested)

Requires [Rust](https://rustup.rs) with the MSVC toolchain (the default on
Windows) and Windows 10 2004+ / Windows 11.

```powershell
cargo run --release
```

The overlay covers the screen, the hole drifts on its own, and clicks pass
through to your apps. Switch the disk look from the tray icon (the ^ overflow
area) — 8 presets from Inferno to Zen. Quit via the tray menu or Esc.

Cross-compiling from WSL also works: `rustup target add x86_64-pc-windows-gnu`,
install `mingw-w64`, then `cargo build --release --target x86_64-pc-windows-gnu`.

### macOS (UNTESTED — help wanted)

The macOS port (ScreenCaptureKit capture + `NSWindowSharingNone` self-exclusion
+ menu-bar presets) is structurally complete and kept type-checked via
`cargo check --target aarch64-apple-darwin`, but has never run on real
hardware — there is no Mac in this project's dev loop.

```sh
cargo run --release   # on a Mac
```

On first launch, grant **Screen Recording** permission (System Settings →
Privacy & Security) and relaunch. If you try it, success or failure reports
are equally welcome as issues.

### Linux

Not yet — neither X11 nor Wayland offers a way to exclude a window from
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

Singularity is a screen-space gravitational-lens approximation, not a full
geodesic integrator. For each pixel it remaps the sampled background coordinate
toward the hole — strong near the event horizon, negligible far away — then
draws a black core, a warm photon ring, and a subtle accretion swirl. The centre
follows a slow Lissajous path so the hole drifts on its own.

## Performance & battery

A desktop overlay runs a continuous capture + render loop, so on a laptop expect
noticeably higher power draw while active. Stage 4 keeps this in check by only
warping the region near the hole and capping the frame rate.

## Credits

- Concept inspired by [ghostty-blackhole](https://github.com/s0xDk/ghostty-blackhole) by s0xDk
- Stop-gap runs on [ShaderGlass](https://github.com/mausimus/ShaderGlass) by mausimus

## License

MIT — see [LICENSE](LICENSE).
