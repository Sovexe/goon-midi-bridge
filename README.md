# Goon MIDI Bridge

Play MIDI files or live MIDI input through Goonstation instruments. Supports held
notes, sustain pedals, channel routing, and velocity adjustment. Windows x64 only.
This is an unofficial companion app.

## Use

Download the Windows ZIP from [Releases](https://github.com/Sovexe/goon-midi-bridge/releases),
extract it, and run **GoonMidiBridge.exe**.

1. Open your instruments in BYOND and restore any minimized instrument windows.
2. Select your BYOND client in the bridge and click **Link Instruments**.
3. Enable keyboard input on each instrument. Use its MIDI badge to choose channels.
4. Open a `.mid` file and click **Play**, or choose a live MIDI input and click **Connect**.

**Auto** velocity adjusts quiet input. Use its offset for fine tuning and
**Recalibrate** to start over. **Manual** applies a fixed multiplier; **Original**
preserves incoming velocities. **Stop All Notes** stops playback and releases notes.

The game supplies the sounds and determines instrument ranges and sustain support.
Notes outside an instrument's range are skipped. Choose instruments and their MIDI
channels yourself. Channel labels show General MIDI instrument names from program
changes and update during playback. Names are available as soon as a file opens.
**Unspecified** means no program was provided; custom sound banks may use different
sounds. The labels do not change the Goonstation instrument you are playing.

Expand **MIDI Channels** to choose what the bridge sends. Click the MIDI status
on an instrument to expand its channel selector. Both start collapsed.
**All** and **None** make selecting a group easier. Channel 10 is normally drums
in General MIDI, so it starts unchecked in the bridge. Enable it if needed;
drum notes will use the selected instrument's sounds.

## Linking

The helper connects to an authenticated WebSocket on `127.0.0.1` and sends ordinary
TGUI note actions. Link again after opening or reloading an instrument, or restarting
the bridge.

## Build

Install [Rust stable](https://rustup.rs/) and Visual Studio Build Tools with
**Desktop development with C++** and a Windows SDK, then run:

```powershell
cargo build --release --locked
```

The executable is written to `target/release/GoonMidiBridge.exe`.
GitHub Actions builds the Windows app and uploads a ZIP containing the executable,
README, and license files.

## License

[MIT](LICENSE), copyright Sovexe. Keep `LICENSE` and `THIRD_PARTY_LICENSES.txt` with
the executable when redistributing it. The latter contains dependency
notices; update it when dependencies change. Goonstation and BYOND have their own
licenses.
