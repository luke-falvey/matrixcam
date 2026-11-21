# matrixcam

Turns your webcam into Matrix-style green digital rain on a black background and
publishes it as a virtual camera you can select in Microsoft Teams (or Zoom, Meet, ...).
Written in Rust; a single ~2 MB binary with no runtime dependencies.

## Virtual camera support

An ordinary program cannot add a camera device by itself; each OS has its own mechanism.

| OS | How | One-time setup |
|----|-----|----------------|
| Windows 11 | Built-in virtual camera API (no OBS, no third-party driver) | None - the first run asks for administrator approval once |
| Linux | v4l2loopback | `sudo modprobe v4l2loopback devices=1 exclusive_caps=1 card_label="Matrix Cam"` |
| macOS | Preview window only for now (Apple requires a signed, notarized camera extension) | - |

On Windows the camera is called **Matrix Cam** and exists only while `matrixcam.exe` is running.
The one-time approval copies a small component to `C:\Program Files\matrixcam` and registers it,
because Windows loads virtual-camera code inside its own camera service.
`matrixcam --uninstall-driver` removes it again. Windows 10 is not supported.

## Use

1. Run `matrixcam` *before* turning your camera on in Teams (only one app can hold the real webcam).
2. In Teams: **Settings > Devices > Camera** and choose **Matrix Cam**.

Options: `--camera 1` pick another webcam, `--list-cameras`, `--preview` also show a window,
`--no-virtualcam` window only, `--cell 12` smaller glyphs / more detail, `--mirror`,
`--width/--height/--fps`, `--device /dev/video10` (Linux). Ctrl+C quits.

### Troubleshooting on Windows

Create an empty file named `debug` in `C:\ProgramData\matrixcam`, run again, and look at
`vcam.log` in the same folder - it records what the camera component did inside Windows' camera service.

## Build

    cargo build --release

Linux needs `libx11-dev libxkbcommon-dev libwayland-dev libxcursor-dev libclang-dev`.
On Windows the build also compiles the `vcam` crate (the camera component) and embeds it in the exe.
Pushing to GitHub runs `.github/workflows/build.yml`, which builds Windows, macOS and Linux binaries.
