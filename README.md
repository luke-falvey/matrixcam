# matrixcam

Turns your webcam into Matrix-style green digital rain on a black background and
publishes it as a virtual camera you can select in Microsoft Teams (or Zoom, Meet, ...).
Written in Rust; a single ~2.5 MB binary with no runtime dependencies.

## Virtual camera support

An ordinary program cannot add a camera device by itself - the OS needs a driver.

| OS | Status | One-time setup |
|----|--------|----------------|
| Linux | implemented (v4l2loopback, no OBS) | `sudo modprobe v4l2loopback devices=1 exclusive_caps=1 card_label="Matrix Cam"` |
| Windows | implemented via the OBS Virtual Camera driver | Install [OBS Studio](https://obsproject.com); it does not need to run |
| macOS | preview window only for now | - |

## Use

1. Run `matrixcam` *before* turning your camera on in Teams (only one app can hold the real webcam).
2. In Teams: **Settings > Devices > Camera** and choose the virtual camera
   ("Matrix Cam" on Linux, "OBS Virtual Camera" on Windows).

Options: `--camera 1` pick another webcam, `--list-cameras`, `--preview` also show a window,
`--no-virtualcam` window only, `--cell 12` smaller glyphs / more detail, `--mirror`,
`--width/--height/--fps`, `--device /dev/video10` (Linux). Ctrl+C quits.

## Build

    cargo build --release

Linux needs `libx11-dev libxkbcommon-dev libwayland-dev libxcursor-dev libclang-dev nasm`.
Pushing to GitHub runs `.github/workflows/build.yml`, which builds Windows, macOS and Linux binaries.
