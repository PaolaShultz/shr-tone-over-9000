# rpi-tone-over-9000

A minimal Raspberry Pi 5 amp-sim experiment: one Rust process, one mono JACK
client, NeuralAmpModelerCore, and one 40×13 terminal screen. It reads local
`.nam` capture files and processes `system:capture_1` to
`system:playback_1` at 48 kHz.

It is not a DAW, plugin host, LV2/CLAP wrapper, TONE3000 client, model
downloader, preset manager, GUI, or WebView. It does not use JUCE, GTK,
WebKitGTK, React, or tone3000.com login. V1 is mono, has no cab/IR/EQ, and keeps
oversampling off.

## Raspberry Pi 5 build

Use 64-bit Debian 12/13. Install native headers and the C/C++ toolchain; do not
install distro Cargo:

```sh
sudo apt install --no-install-recommends \
  build-essential pkg-config libjack-jackd2-dev libasound2-dev \
  nlohmann-json3-dev libeigen3-dev
rustup toolchain install 1.97.1 --profile minimal --component rustfmt,clippy
```

The current tone3000-plugin repository stores NeuralAmpModelerCore at
`plugin/NeuralAmpModelerCore` as a submodule (not the older documented `libs/`
path). Clone it once; `vendor/` remains local and ignored:

```sh
git clone --no-recurse-submodules \
  https://github.com/tone-3000/tone3000-plugin.git vendor/tone3000-plugin
git -C vendor/tone3000-plugin submodule update --init \
  plugin/NeuralAmpModelerCore
cargo build --locked
```

`build.rs` compiles only the core's `NAM/*.cpp` files plus the small C ABI
bridge. The wrapper, JUCE, UI, tests, tools, and AudioDSPTools are not compiled.
The upstream core now uses Eigen as well as nlohmann/json; both are found via
`pkg-config`. Missing headers are a setup error, not a reason to substitute a
plugin or GUI mechanism.

The Rust pin is copied exactly from `../shr-daw/rust-toolchain.toml`. Never
silently move it. Before any future combined validation pass, record
`rustc -vV`, then run `cargo check --locked` and `cargo build --locked` in that
order.

## JACK and the USB interface

Use a USB Audio Class-compliant guitar interface. This program never starts,
stops, restarts, or reconfigures JACK; it opens with `NO_START_SERVER` and
attaches only to the requested ports. Start the already-owned JACK service at
48 kHz, 256 frames, and three periods, then run:

```sh
target/debug/rpi-tone-over-9000
```

For a JACK server already configured at 128 frames:

```sh
target/debug/rpi-tone-over-9000 --period 128
```

`--period` validates JACK's live period. It does not seize server ownership or
change timing during a session. Override unusual exact routes with
`--capture-port` and `--playback-port`; defaults are
`system:capture_1` and `system:playback_1`.

Follow `../shr-daw/scripts/setup.sh` and
`../shr-daw/docs/INSTALLATION.md` for JACK ownership, the audio-group
`rtprio`/`memlock` recipe, and optional CPU isolation. Keep the distribution's
working real-time policy. Add the helper-owned limits file only when live
limits are insufficient. Do not create a competing JACK service. Three periods
remain the safe USB default; earn 128-frame operation with sustained zero-xrun
measurement.

At 48 kHz, one 256-frame period is 5.33 ms and one 128-frame period is 2.67 ms;
USB, period count, converters, and the model add to end-to-end latency. These
period durations are not a measured round-trip claim.

## Models and controls

Put local `.nam` files in `./models` or pass `--models-dir DIR`. The directory
is watched, the list wraps, and loading/prewarming happens outside the JACK
callback. A loaded model must be mono and must either expect 48 kHz or leave its
expected rate unknown. Other extensions are not listed and the loader rejects
non-`.nam` paths.

- Up/Down or `+`/`-`: select a model
- Enter: load selected model
- `r`: reload current model
- `i` / `o`: raise input/output gain by 0.5 dB; Shift uses 3 dB
- `b`: toggle NAM bypass
- `m`: learn the next positional CC for the focused control
- `?`: help; `q` or Esc: clean exit

Tap the model row to load. Tap a parameter to focus it; vertical dragging on
input/output changes gain in 0.5 dB steps. Touch is ordinary terminal mouse
input. The terminal remains the existing 480×320 tty using
Uni2-TerminusBold24x12 at 40×13 cells; this project does not change the font.

The final row is owned only by the shared status renderer: a steady white `■`,
one space, then model, xrun count, JACK period, MIDI device, JACK CPU estimate,
and thermal temperature. Faults temporarily replace that text. The two rows
above it are controller hints; there are no stacked gray status rows.

## MiniLab / ALSA MIDI

MIDI is optional. Copy only the verified public shape from `controller.conf`
and fill `input=`, `profile=`, encoder fields, and the needed `rotary.N=CC`
values from the existing shr-daw setup. Do not publish private device names or
learned mappings. `--midi "exact stable ALSA identity"` overrides `input=`.

Read `../shr-daw/docs/CONTROLLER_INTERFACE.md` and
`../shr-daw/docs/CONTROLLER_PROFILES.md`; they remain the policy owners. This
app preserves their v9 config shape and exact stable ALSA identity matching.
Rotary 1 browses and its press loads. Rotaries 2/3 control input/output gain,
4 controls bypass, and 5 is consumed while oversampling remains off. The
configured MiniLab rotaries use shr-daw's direction-only relative convention,
so they carry the current value without stale-position jumps. Session MIDI
learn accepts a positional CC and blocks it until the physical value reaches
or crosses the loaded value; each successful model load re-arms pickup.

All MIDI note-on, note-off, velocity-zero release, and pressure messages are
consumed locally. This process creates no MIDI output and forwards no notes, so
shutdown sends no All Notes Off. TUI/touch controls always remain available
when MIDI is absent or offline.

Parameter indicators use the shr-daw thresholds verbatim: green below the
loaded value by more than 0.03, bright yellow within ±0.03, and red above it by
more than 0.03.

## CPU, cooling, and diagnostics

The planning estimate for typical NAM inference is roughly 15–25% of one Pi 5
core at 48 kHz mono. It is model-dependent and not acceptance evidence. The
status-row CPU number is JACK's whole-graph running estimate. Two-times
oversampling can roughly triple NAM CPU and is unnecessary for this v1, so it
is fixed off.

A passive heatsink is mandatory; the official Raspberry Pi Active Cooler is
recommended. Sustained inference can thermal-throttle an inadequately cooled
Pi 5. Inspect temperature separately with:

```sh
vcgencmd measure_temp
```

## Troubleshooting

- **Build cannot find JACK, Eigen, or nlohmann/json:** install the named `-dev`
  packages above. WebKitGTK/GTK/JUCE are not dependencies.
- **JACK will not open:** start the existing owner; this app deliberately will
  not auto-start a server. Confirm 48 kHz and the selected 256/128 period.
- **No audio:** load a valid model, check that bypass is not the only expected
  path, and inspect the two exact JACK connections and interface input gain.
- **Wrong sample rate:** restart/configure the owning JACK service at 48 kHz;
  neither the app nor the core performs live resampling.
- **Xruns:** return to 256 frames/three periods, stop background load, verify
  live `rtprio`/`memlock`, then use shr-daw's reviewed CPU-tuning diagnostics.
- **MIDI not detected:** the app still works from touch/keys. Verify the exact
  stable ALSA input name and the `input=`/`--midi` value; ambiguous names are
  refused rather than guessed.

Software compilation alone cannot establish zero xruns, physical interface
behavior, touch calibration, controller acceptance, thermal headroom, or
listening quality on the target Pi.
