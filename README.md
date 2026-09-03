# rpi-tone-over-9000

A minimal Raspberry Pi 5 amp sim: one Rust process, one mono JACK client,
NeuralAmpModelerCore, cabinet convolution, and one 40×13 terminal screen. It
chains up to four local `.nam` captures and `.wav` cabinet IRs in any order and
processes `system:capture_1` to `system:playback_1` at 48 kHz. The normal chain
is **pedal NAM → amp-head NAM → cabinet IR**.

It is not a DAW, plugin host, LV2/CLAP wrapper, preset manager, GUI, or WebView.
It does include a small command-line model manager with a pinned, checksummed
starter catalog, safe local/URL import, verification, and links into the
official TONE3000 browser. It does not embed a tone3000.com login. V1 is mono,
has no EQ, and keeps oversampling off.

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

Or use the repository installer. It can install Debian prerequisites, fetch
the NAM core, build the release binary, and populate the user model library:

```sh
scripts/install.sh --system-deps --accept-t3k
```

`--accept-t3k` acknowledges that the curated TONE3000 captures are downloads
for local use and must not be redistributed. Without it, the installer still
installs the five CC0 cabinet IRs. Run `scripts/install.sh --help` to choose a
different prefix or library directory.

`build.rs` compiles only the core's `NAM/*.cpp` files plus the small C ABI
bridge. The wrapper, JUCE, UI, tests, tools, and AudioDSPTools are not compiled.
The upstream core now uses Eigen as well as nlohmann/json; both are found via
`pkg-config`. `src/eigen_compat.h` bridges the core's Eigen 5 `lastN` spelling
to the Eigen 3.4 API shipped by Debian 12/13. Missing headers are a setup error,
not a reason to substitute a plugin or GUI mechanism.

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

## Models, cabinets, and controls

The default library is
`$XDG_DATA_HOME/rpi-tone-over-9000/models`, falling back to
`$HOME/.local/share/rpi-tone-over-9000/models`. Override it with
`RPI_TONE_MODELS_DIR` or `--models-dir DIR`. The directory is watched and the
browser wraps.

The built-in catalog contains five pedal NAMs, five amp-head NAMs, five
amp-plus-cab rig NAMs, and five cabinet IRs. Install and inspect it with:

```sh
rpi-tone-over-9000 models list
rpi-tone-over-9000 models install all --accept-t3k
rpi-tone-over-9000 models verify all
rpi-tone-over-9000 models browse cab
```

`models browse pedal|amp|cab|rig` opens the filtered official TONE3000 search
when a desktop browser is available and always prints the URL. TONE3000's full
in-app API flow requires a registered OAuth client and redirect URI, so the app
does not pretend to have account access. Download a chosen `.nam` or `.wav`
asset from its source, then add it safely:

```sh
rpi-tone-over-9000 models import ~/Downloads/my-amp.nam
rpi-tone-over-9000 models add-url https://example.org/my-cab.wav \
  --name my-cab.wav --sha256 EXPECTED_SHA256
```

Imports and downloads are staged outside the watched directory, checksummed
when a digest is available, fully decoded, and test-loaded before an atomic
rename. Existing files are preserved unless `--replace` is explicit. A bad
download therefore cannot replace the active library or sounding chain.

For an IR pack with documented microphone captures, record the pack identity
while importing each WAV. Files with the same `--cab-id` become variants of one
virtual microphone selector:

```sh
rpi-tone-over-9000 models import mesa-sm57-edge.wav \
  --cab-id mesa-412-v30 --cabinet "Mesa 4x12" --speaker V30 \
  --mic SM57 --position "cap edge" --variant "57 edge"
rpi-tone-over-9000 models import mesa-r121-cone.wav \
  --cab-id mesa-412-v30 --cabinet "Mesa 4x12" --speaker V30 \
  --mic R121 --position cone --variant "121 cone"
```

The screen always shows `PEDAL > AMP > CAB > MIC*`. `MIC*` is a virtual fourth
stage: focus it with Left/Right and use Up/Down to swap among installed IRs
with the same cabinet ID. Pack variants are prepared together when the cabinet
is loaded; MIC changes then send only a real-time-safe variant index to the
audio thread instead of rebuilding the amp or convolution. The cab and
microphone remain one convolution and consume one real DSP slot. Unknown user
IRs still show `MIC UNKNOWN` rather than inventing capture details. The
current CC0 packs document cabinet/speaker families and voicing names but not
their microphone or position, so they honestly display `UNSPECIFIED` and
`UNDOCUMENTED` until a better documented pack is installed.

A chain has up to four ordered slots. The whole edited chain is loaded and
prepared outside the JACK callback, then published atomically. A failed edit
leaves the sounding chain intact. NAM captures must be mono and must either
expect 48 kHz or leave their expected rate unknown. Cabinet WAVs must be 48 kHz
mono or stereo PCM/float and at most two seconds; stereo IRs are downmixed to
mono. Convolution setup and allocation happen off the audio thread.

- Up/Down or `+`/`-`: select a model
- Left/Right: focus a chain slot or its virtual `MIC*` stage
- Up/Down while on `MIC*`: atomically select another variant of the same cab
- `a`: add selected model after the focused slot
- Enter: replace the focused slot (or fill an empty chain)
- `d`, Delete, or Backspace: remove the focused slot
- `[` / `]`: move the focused slot earlier/later
- `r`: reload the whole chain
- `i` / `o`: raise input/output gain by 0.5 dB; Shift uses 3 dB
- `b`: toggle the whole-chain bypass
- `m`: learn the next positional CC for the focused control
- `?`: help; `q` or Esc: clean exit

Tap the model row to replace the focused slot. The dedicated chain-control row
focuses, adds, and deletes slots. Tap a parameter to focus it; vertical dragging
on input/output changes gain in 0.5 dB steps. Touch is ordinary terminal mouse
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
Rotary 1 browses and its press replaces the focused slot. Rotaries 2/3 control input/output gain,
4 controls bypass, and 5 is consumed while oversampling remains off. The
configured MiniLab rotaries use shr-daw's direction-only relative convention,
so they carry the current value without stale-position jumps. Session MIDI
learn accepts a positional CC and blocks it until the physical value reaches
or crosses the loaded value; each successful chain edit re-arms pickup.

All MIDI note-on, note-off, velocity-zero release, and pressure messages are
consumed locally. This process creates no MIDI output and forwards no notes, so
shutdown sends no All Notes Off. TUI/touch controls always remain available
when MIDI is absent or offline.

Parameter indicators use the shr-daw thresholds verbatim: green below the
loaded value by more than 0.03, bright yellow within ±0.03, and red above it by
more than 0.03.

## CPU, cooling, and diagnostics

The planning estimate for typical NAM inference is roughly 15–25% of one Pi 5
core per NAM slot at 48 kHz mono. Chain CPU is approximately additive and
remains model-dependent; cabinet convolution adds its own smaller,
IR-length-dependent cost. The status-row CPU number is JACK's whole-graph
running estimate. Two-times
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
- **Harsh/fizzy amp-head sound:** add a cabinet `.wav` after the amp. Do not use
  an amp-head capture alone as a finished guitar signal.
- **No audio:** load a valid pedal → amp → cab chain, check bypass, and inspect
  the two exact JACK connections and interface input gain.
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
