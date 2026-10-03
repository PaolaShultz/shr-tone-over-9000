# SHR Tone Over 9000 operating guide

[Project overview and quick start](../README.md). Run commands from the repository root.

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

The exact Rust version is selected by this repository’s `rust-toolchain.toml`. Before any future combined validation pass, record
`rustc -vV`, then run `cargo check --locked` and `cargo build --locked` in that
order.

## JACK and the USB interface

Use a USB Audio Class-compliant guitar interface. This program never starts,
stops, restarts, or reconfigures JACK; it opens with `NO_START_SERVER` and
attaches only to the requested ports. Start the already-owned JACK service at
48 kHz, either 128 or 256 frames, and three periods, then run:

```sh
target/debug/shr-tone-over-9000
```

The app detects the live JACK period automatically. To require a particular
period in a script or diagnostic check, pass it explicitly:

```sh
target/debug/shr-tone-over-9000 --period 128
```

`--period` is optional and validates JACK's live period when supplied. It does
not seize server ownership or change timing during a session. Override unusual
exact routes with `--capture-port` and `--playback-port`; defaults are
`system:capture_1` and `system:playback_1`.

With JACK2, put the global `--sync` option before `-d alsa`. Its default
asynchronous graph mode adds one period between this client's processed output
and the ALSA playback cycle. At 48 kHz/128 frames, synchronous mode therefore
reports 128 capture plus 128 playback frames rather than 128 plus 256.

See the [SHR-DAW installation guide](https://github.com/PaolaShultz/shr-daw/blob/main/docs/INSTALLATION.md)
for JACK ownership and the audio-group `rtprio`/`memlock` recipe. Keep the distribution's
working real-time policy. Add the helper-owned limits file only when live
limits are insufficient. Do not create a competing JACK service. Three periods
remain the safe USB default; earn 128-frame operation with sustained zero-xrun
measurement.

At 48 kHz, one 256-frame period is 5.33 ms and one 128-frame period is 2.67 ms;
USB, period count, converters, and the model add to end-to-end latency. These
period durations are not a measured round-trip claim.

## Models, cabinets, and controls

The default library is
`$XDG_DATA_HOME/shr-tone-over-9000/models`, falling back to
`$HOME/.local/share/shr-tone-over-9000/models`. Override it with
`RPI_TONE_MODELS_DIR` or `--models-dir DIR`. The directory is watched and the
browser wraps.

The built-in catalog contains five pedal NAMs, five amp-head NAMs, five
amp-plus-cab rig NAMs, and five cabinet IRs. Install and inspect it with:

```sh
shr-tone-over-9000 models list
shr-tone-over-9000 models install all --accept-t3k
shr-tone-over-9000 models verify all
shr-tone-over-9000 models browse cab
```

`models browse pedal|amp|cab|rig` opens the filtered official TONE3000 website
when a desktop browser is available and always prints the URL. Download a
chosen `.nam` or `.wav` asset manually, then add it safely:

```sh
shr-tone-over-9000 models import ~/Downloads/my-amp.nam
shr-tone-over-9000 models add-url https://example.org/my-cab.wav \
  --name my-cab.wav --sha256 EXPECTED_SHA256
```

Imports and downloads are staged outside the watched directory, checksummed
when a digest is available, fully decoded, and test-loaded before an atomic
rename. Existing files are preserved unless `--replace` is explicit. A bad
download therefore cannot replace the active library or sounding chain.

### Smart TONE3000 search

The normal on-device flow needs no hub commands:

1. Press `h` from the amp screen.
2. On first use, confirm the installation's TONE3000 publishable client ID and
   press Enter.
3. Scan the QR shown directly in the terminal with a phone on the Pi's current
   Wi-Fi and approve the login. The authorization URL stays internal; the only
   alternate action on this screen is Esc to cancel.
4. Type or paste a query such as
   `marshall jcm with bass 3-4 mid 6+ high 2-3`, then press Enter.
5. Use Up/Down for exact model results, Enter for all available metadata and
   evidence, and Left/Right for another bounded result page.
6. Press `d` to download only the highlighted model. After validation, the amp
   screen returns with that file selected; press Enter separately to load it.

Search, authentication, and download run outside the TUI/audio event loop, so
meters and the sounding chain remain live. Esc cancels OAuth immediately and a
search after its current HTTP request; input is retained for retry. A started
model download finishes its staged validation and atomic publish rather than
leaving a partial library entry. Pressing `d` for an already-installed hub
model simply selects the existing file.

The TUI automatically proposes
`http://CURRENT_PRIVATE_WIFI_IP:43900/callback`. Register that exact URI in the
TONE3000 client before authorizing; if the Pi gets a different address on
another Wi-Fi, update the allowed redirect. `TONE3000_REDIRECT_URI` can pin an
explicit registered private-LAN callback. During the five-minute login window,
the QR points to a short local URL on the Pi; that listener redirects the phone
to TONE3000 and then accepts only the registered callback path.

The publishable client ID is installation configuration, never a compiled
default. Set it while installing:

```sh
scripts/install.sh --tone3000-client-id t3k_pub_YOUR_KEY
```

This writes owner-only
`$XDG_CONFIG_HOME/shr-tone-over-9000/hub.conf`, falling back to
`$HOME/.config/shr-tone-over-9000/hub.conf`. Another installation supplies its
own ID. `TONE3000_CLIENT_ID` and `RPI_TONE_HUB_CONFIG` remain deployment
overrides. Never put the TONE3000 secret key on this device.

The equivalent CLI flow is retained below.

Create a TONE3000 publishable API key and register the callback URI in its
allowed redirects. Connect once; the default callback is suitable when the
browser runs on the same machine:

```sh
shr-tone-over-9000 models hub connect --client-id t3k_pub_YOUR_KEY
```

On a headless Pi, use its private LAN address. The CLI prints only the short
Pi-local handoff URL, which must be opened on a phone connected to the same LAN:

```sh
shr-tone-over-9000 models hub connect --client-id t3k_pub_YOUR_KEY \
  --redirect-uri http://192.168.1.50:43900/callback
```

Credentials are stored at
`$XDG_CONFIG_HOME/shr-tone-over-9000/tone3000-auth.json`, falling back to
`$HOME/.config/shr-tone-over-9000/tone3000-auth.json`, with mode 0600. Override
the location with `RPI_TONE_HUB_AUTH` or `--auth-file`. The secret TONE3000 key
is server-only and must never be supplied to this application.

Search amp and capture settings using one query. `high`/`highs` is normalized
to `treble`, and `middle`/`mids` to `mid`:

```sh
shr-tone-over-9000 models hub search \
  "marshall jcm with bass on 3-4 and mid on 6+ and high on 2-3"
```

The command first prints its parsed make/model and numeric constraints. It then
searches tone metadata, reads the model names and explicitly model-associated
description lines inside the returned tones, and prints only models whose
documented settings satisfy every constraint. Supported syntax is `bass 4`,
`bass 3-4`, `mid 6+`, `gain >=6`, and `treble <=3`; numeric filters use a 0–10
control scale. Settings are not guessed. An unknown setting does not match a
constrained field, and zero exact matches remains zero rather than silently
broadening the query.

These values are search metadata only. They do not create runtime amp controls,
interpolate between captures, switch a capture pack, or download unselected
models.

Results are model-scoped handles. Inspect and download exactly one:

```sh
shr-tone-over-9000 models hub show t3k:model:88421
shr-tone-over-9000 models hub download t3k:model:88421
```

Search and show retrieve JSON metadata only. Only `hub download` fetches binary
model data, and it accepts exactly one `t3k:model:ID`, never a tone/pack ID. The
download is limited to 100 MiB, staged, test-loaded, hashed, and atomically
installed. Its TONE3000 identity, creator, license, source, parsed settings,
evidence, and SHA-256 are recorded in
`.rpi-tone-over-9000-models.json` beside the models. Use `--name FILE.nam` to
choose a local filename or `--replace` to replace an existing one explicitly.

The hub defaults to NAM architecture 2. Pass `--architecture 1` or
`--architecture custom` only when that architecture is supported by the pinned
NAM core. Use `--page N` to request another bounded page. TONE3000 heavily
rate-limits custom search and may require integration approval; the application
does not scrape the website or bypass API terms. See the current
[TONE3000 API documentation](https://www.tone3000.com/api).

For an IR pack with documented microphone captures, record the pack identity
while importing each WAV. Files with the same `--cab-id` become variants of one
virtual microphone selector:

```sh
shr-tone-over-9000 models import mesa-sm57-edge.wav \
  --cab-id mesa-412-v30 --cabinet "Mesa 4x12" --speaker V30 \
  --mic SM57 --position "cap edge" --variant "57 edge"
shr-tone-over-9000 models import mesa-r121-cone.wav \
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
- `h`: open smart hub connect/search/details/download
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

## Moving the Pi to another Wi-Fi

This Debian/Raspberry Pi setup uses NetworkManager. The easiest terminal UI is:

```sh
sudo nmtui
```

Choose **Activate a connection**, select the friend's network, enter its
password, and quit. The direct command is also short and prompts securely for
the password instead of putting it in shell history:

```sh
nmcli device wifi list
sudo nmcli --ask device wifi connect "FRIEND SSID" ifname wlan0
```

Changing the active Wi-Fi disconnects an SSH session carried by the old
network, so do this from the Pi's own keyboard/display or expect to reconnect at
its new address. Check it with `ip -brief address show wlan0`. If TONE3000 OAuth
uses a phone, put the phone on the same Wi-Fi and allow the newly displayed Pi
callback URI in the TONE3000 client.

## MiniLab / ALSA MIDI

MIDI is optional. Copy only the verified public shape from `controller.conf`
and fill `input=`, `profile=`, encoder fields, and the needed `rotary.N=CC`
values from the existing shr-daw setup. Do not publish private device names or
learned mappings. `--midi "exact stable ALSA identity"` overrides `input=`.

See the SHR-DAW [controller interface](https://github.com/PaolaShultz/shr-daw/blob/main/docs/CONTROLLER_INTERFACE.md)
and [controller profiles](https://github.com/PaolaShultz/shr-daw/blob/main/docs/CONTROLLER_PROFILES.md). This
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

## Rename compatibility

The repository, checkout and executable are `shr-tone-over-9000`. New model
and account directories use that name. If the new path is absent, the app
continues to use an existing `rpi-tone-over-9000` model or account path.
Explicit `RPI_TONE_MODELS_DIR`, `RPI_TONE_HUB_CONFIG` and `RPI_TONE_HUB_AUTH`
overrides remain supported. Library metadata filenames
`.rpi-tone-over-9000-ir.tsv` and `.rpi-tone-over-9000-models.json` stay stable so
existing IR descriptions and model provenance remain intact. These are format
compatibility names, not stale repository links.

To move an existing installation to the new default layout, close the app,
move its directories under `~/.local/share/` and `~/.config/` to
`shr-tone-over-9000` (only if those destinations do not exist), then run
`scripts/install.sh --no-models`. This preserves model files and credentials.
