# rsttherm

This is my attempt to get the Thermal Master P3 thermal camera running on Linux, with:

- a driver (based on [jvdillon/p3-ir-camera](https://github.com/jvdillon/p3-ir-camera); see [Credits](#credits))
- a CLI and HTTP API to retrieve images programmatically
- a live viewer GUI (see [Viewer](#viewer))

## Install

```sh
make all                           # dependencies, then build and install
make install                       # build, install to ~/.local/bin and add the udev rule
make install PREFIX=/usr/local     # ... to /usr/local/bin instead
make uninstall                     # remove the binary, the udev rule and saved settings
make run                           # run the viewer from the source tree
```

Or build directly with Cargo:

```sh
cargo build --release              # CLI + HTTP + viewer
cargo build --release --no-default-features   # headless: no viewer, no display libraries
```

Installation may prompt for `sudo` access: the camera is not a webcam and does not
appear as `/dev/video*`. Accessing it over raw USB requires installing a `udev` rule
(use `make udev` to install just the rule). `make uninstall` removes the rule.

## CLI

```
rsttherm list
rsttherm [-d DEVICE] info
rsttherm [-d DEVICE] measure [-p X,Y] [-e EMISSIVITY] [--reflected °C] [--offset °C]
rsttherm [-d DEVICE] shutter
rsttherm serve [--port 8080] [--bind 127.0.0.1] [-e ...] [--reflected ...] [--offset ...]
rsttherm gui
```

`DEVICE` is a hexadecimal `VID:PID` (e.g. `3474:45a2`), or part of the serial number or model name.
If omitted, the first camera found is used.

`measure` starts the stream (about 3 s), reads one frame and prints `min`, `max`,
`mean`, `center` and optionally `point`, one per line in °C.
Pixel coordinates refer to the sensor, with `(0, 0)` at the top left.

Exit codes: `0` success, `8` invalid syntax, `16` command failed.

## HTTP API

All endpoints use `GET` and return plain-text responses. The server binds to loopback by default.

```
GET /devices
GET /measure                 # same lines as `rsttherm measure`
GET /measure?x=128&y=96      # ... plus that pixel
GET /shutter
```

Status codes: `200` success, `400` bad request, `500` failure (no frame yet, or a point outside the
image). `rsttherm gui` also serves this API, using the viewer's correction settings.

## Viewer

- Camera picker at the top of the sidebar, listing connected cameras by model and
  serial number. `-d` selects the initial camera.
- Palettes: ironbow, rainbow, inferno, white hot, black hot, with a colour bar.
- Colour range: automatic (ignores the hottest and coldest 1 % so a single hot pixel
  doesn't flatten the scene), or fixed and adjusted using `Manual`.
- Markers for the centre, the hottest pixel and the coldest pixel. Click the image to add spot
  meters; right-click a spot meter to remove it. Hovering shows the temperature under the cursor.
- Correction: emissivity, reflected temperature, and a calibration offset added to every
  reading.
- Shutter/NUC button for a flat-field calibration on demand.
- Zoom up to 8× with the mouse wheel or a pinch, around the pointer; drag to pan.
  The −, + and Fit buttons and the zoom factor appear over the image while the pointer is
  on it. Spots and markers follow the zoom.
- Noise reduction (Off, Low, High) averages each pixel over several frames to reduce
  sensor flicker. A pixel that changes by more than 0.5 K is shown immediately, so
  moving objects don't smear. Noise reduction applies to the viewer only; `measure` and the HTTP API
  report raw frames.
- Rotate and mirror.
- Snapshot: the image with its overlays as PNG, and the temperatures as CSV (noise
  reduction included), written to the working directory.

Settings and the window position are remembered across restarts.

## Notes on the hardware

- Readings represent the camera's apparent temperature (1/64 K per count) and assume a black
  body. Emissivity is corrected with the Stefan–Boltzmann approximation; the offset
  can compensate for remaining error.
- The camera runs a shutter calibration by itself about every 90 s, with an audible
  click. The frame immediately after a shutter calibration is malformed and is dropped.
- Allow a few minutes of warm-up before trusting absolute readings.
- The camera runs in high-gain mode, covering −20 to 150 °C. It also has a low-gain mode for up to
  550 °C, but on firmware 00.00.02.18 its readings are about 57 K too low at room
  temperature. No conversion for these readings is known, so rsttherm doesn't offer this mode.

## Credits

The USB protocol (command bytes, the start-up sequence with its delays, frame layout
and resynchronisation) was reverse-engineered in
[p3-ir-camera](https://github.com/jvdillon/p3-ir-camera) (Apache-2.0) and the
discussion in its [issue #2](https://github.com/jvdillon/p3-ir-camera/issues/2). The
driver here is a fresh Rust implementation of that protocol.
