# Architecture

> **Last reviewed:** 2026-10-10 · **Last updated:** 2026-10-14 · **Change:** minor (the VST3 scan cache names the scanning code and retries a timeout once; a failing helper loads in-process only bundles known to load; previously minor: one plugin-scan startup call for both apps; previously minor: the VST3 scan is cached per bundle across runs, in the settings folder the engine now resolves for both apps; previously minor: the apps scan VST3 plugins in helper processes, their own executable run with `--soundcraft-scan-vst3`; previously trivial: macos also runs the hosts' main-queue jobs, the CLAP host's too, and records app activation) · **Target:** Avid Pro Tools Ultimate 2026.4.1

SoundCraft is a Cargo workspace of small crates with enforced layering (`cargo xtask layers`).
Nothing below the UI knows about egui, so the interface can be replaced.

The UI package pins `craft-ui` for common icon-button and track record-enable interactions.
SoundCraft supplies each state's colors, border, radius and icon painter; selection and command
dispatch remain in the toolbar and track controls. The single-shape frame option preserves the
existing fill/border blending when controls are disabled. Specialized transport Play/Stop/Record styling,
nonlinear meters, faders, pan gestures, fonts and the live theme resolver remain local. Shared adapters read the current palette. The UI
requires Rust 1.95 or later without raising the core workspace's Rust 1.90 declaration.

```
L0  time        audio-io        midi        video   (standalone: no workspace deps)
    macos                                           (standalone: macOS app integration: files macOS
                                                     opens with the app, the plugin editor window,
                                                     main-queue jobs, app activation)
L1  dsp         clap-host   vst3-host   au-host     (plugins; the three hosts and macos are the
                                                     only crates allowed `unsafe`, confined to their
                                                     FFI modules)
L2  model                                       (the session document)
L3  mix                                         (the mix engine)
L4  engine      playback                        (commands, undo, I/O · audio devices)
L5  automation                                  (MCP server, control-channel client)
L6  ui-egui                                     (the user interface)
    apps: soundcraft (desktop) · soundcraft-cli · soundcraft-web (wasm)
```

## The document

`soundcraft_model::Session` is plain data (serde): tracks with playlists of clips, mixer state
(inserts, sends, routing, automation lanes), tempo and meter maps, memory locations, groups,
busses, and `EditState` (selection, tools, modes, view flags). Decoded audio lives in a
`SourcePool` of `Arc`s that is not serialised; cloning a session is cheap. Sessions save as
`.scraft` JSON plus an `Audio Files/` folder.

## Commands and undo

Every user action is a `CommandSpec` (id, label, menu path, shortcut, params doc, enabled, run)
registered in `crates/engine/src/cmd/`. `Engine::execute(id, params)` runs it, catches any escaped
panic, and pushes an undo snapshot (the previous `Arc<Session>`) when the document changed.
Continuous gestures use `execute_merged` so a fader drag is one undo step. Menus are built from
the incumbent's menu catalog (`crates/engine/catalog/menus.txt`); a menu item lights up when a
command has the same menu path and label (or an alias maps it), which also drives the parity
report.

## Audio

`soundcraft_mix::MixEngine::render(session, pos, frames, out)` renders one block: clips (gain,
fades, clip effects) → trim → inserts → pre-fader sends → fader/mute (automation, VCA, trim
automation) → post-fader sends → pan → busses → aux inputs → master faders. Independent strips
process in parallel; plugin delay compensation aligns every path, the master faders' inserts included. The same engine renders
bounces offline (`RangeRender`, streamed to the file block by block) and realtime playback
(`soundcraft_playback::Player`, which owns it on the audio thread and receives new session
snapshots through a channel).

Device I/O today is the system's *default* output and *default* input device, opened as two
separate cpal streams; the mix renders 512-sample blocks and resamples to the device rate when
they differ. There is no device or buffer-size selection, no ASIO, and no MIDI device layer yet
(see [`hardware-parity.md`](hardware-parity.md)). Sources are decoded whole into the `SourcePool`
and recordings are captured in memory until stop.

`soundcraft-video` holds our own H.264, ProRes and Motion JPEG decoders and the ISO-BMFF demuxer
for the video track. Hosted plugins (`clap-host`, `vst3-host`, `au-host`) are scanned, created and
destroyed off the audio thread; only `process` runs on it.

VST3 scans run out of process in the desktop app and the CLI. Each calls
`soundcraft_engine::plugin_scan_startup` first thing in `main`, before logging, windows or audio:
it answers `<its executable> --soundcraft-scan-vst3 <bundle>` when started as a scan helper, and
otherwise registers its own executable (`/proc/self/exe` on Linux) as the helper. `vst3-host` then loads each bundle in a helper of its
own, eight at a time, and reads back the plugins as one JSON line. A helper still running after
30 s is killed, and its bundle, like one whose helper crashed or failed, is logged, skipped and
not tried again until a rescan. SoundCraft's own process loads only the bundles of the plugins it
creates. Each bundle's outcome is cached across runs in the settings folder
(`soundcraft_engine::settings`, shared by both apps: `plugin-cache/vst3.v1.json`) under a
fingerprint of the bundle and named after the scanning code, so later runs start helpers only for
new or changed bundles and an update starts cold. A timeout is retried once on the next run
before it is cached as a skip. A helper that fails on SoundCraft's side (cannot start, or speaks
another scan protocol) skips nothing: the bundles the cache knows to load are loaded in-process
instead, the others listed as not scanned. CLAP scans still load every plugin binary in-process;
the Audio Unit scan loads none (it only lists the system's component registry).

The cpal callbacks mark their thread (`soundcraft_playback::mark_audio_thread`). Code that may run
there can ask `on_audio_thread()` before doing anything that blocks; the desktop app's logger does,
so a `log::` record from the audio thread (a stream error, a full synth event queue, a hosted
plugin's failed `process`, a CLAP plugin's own log call) is kept without waiting and written later
by the UI thread (see README › Logs).

## Agent control

The desktop app's control channel (JSON lines over TCP) exposes engine commands, inspection,
synthetic input and screenshots; `soundcraft-cli mcp` wraps either a headless engine or a running
app as an MCP server. See `control-protocol.md` and `mcp.md`.

## Revision history

| Date | Change | Summary |
|---|---|---|
| 2026-10-14 | minor | The VST3 scan cache is named after the scanning code (an update starts cold) and retries a timeout once; a failing helper loads in-process only bundles known to load |
| 2026-10-14 | minor | One startup call (`plugin_scan_startup`) for both apps; a helper failing on SoundCraft's side falls back to in-process loads; the Audio Unit scan loads no binary (corrected) |
| 2026-10-14 | minor | The VST3 scan is cached per bundle across runs; the settings folder rule moved into the engine (`settings`) so the CLI uses it too |
| 2026-10-14 | minor | The apps scan VST3 plugins in helper processes (their own executable with `--soundcraft-scan-vst3`), so a plugin that hangs or crashes while scanned is skipped |
| 2026-10-14 | trivial | macos also runs the main-queue jobs the plugin hosts (CLAP too) use to tear down editors dropped off the main thread, and records app activation |
| 2026-10-14 | minor | macos holds the plugin editor window the VST3 and AU hosts share (they depend on it, L1 on L0) |
| 2026-10-14 | minor | Renamed macos-open to macos (soundcraft-macos), the macOS app-integration crate |
| 2026-10-13 | minor | Added macos-open (files opened from Finder, the Dock or `open -a` reach the desktop app) to the layer diagram and the `unsafe` exceptions |
| 2026-10-13 | trivial | delay compensation includes the master faders' inserts (#351) |
| 2026-10-10 | minor | Added video, vst3-host and au-host to the layer diagram; documented device I/O limits and in-memory media |
| 2026-10-05 | major | First architecture overview |
