# Continuous integration

[The workflow](../.github/workflows/ci.yml) runs on pushes to `main`, pull
requests and manual dispatch. It uses read-only repository permissions, cancels
superseded runs and selects exact Rust 1.97.1 with locked Cargo dependencies.

The build needs NeuralAmpModelerCore outside Cargo.lock. CI checks out exact upstream revision 1f42f88535884450104b8711d7595019afa0495b, matching the inspected local core, directly into the path build.rs expects. Only NAM source, its bundled nlohmann header and upstream example_models test fixtures are selected; JUCE, plugin submodules, model catalog downloads and the installation helper are not invoked. Eigen/nlohmann, JACK and ALSA development dependencies support the native build. Normal tests use generated signals and upstream example models without audio devices.

The workflow contains the exact reproducible commands. Historical/exhaustive
auditions and benchmarks remain opt-in. No physical audio, MIDI, DMX, playback,
service activation, media download or deployment is part of these checks.
Compilation and synthetic tests do not establish Raspberry Pi hardware acceptance.
Clippy with warnings denied and release builds are not added as new CI gates.
