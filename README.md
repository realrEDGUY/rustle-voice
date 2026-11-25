# Rust Voicechat

A voice chat server and GUI client in Rust, with several channels per server and
connections to several servers at once.

- **Transport:** QUIC (quinn + rustls/ring) on one UDP port. The control channel is one
  bidirectional stream of length-prefixed JSON; voice uses QUIC datagrams. TLS 1.3 is used
  throughout, with trust-on-first-use (TOFU) certificate pinning.
- **Codec:** IMA-ADPCM in pure Rust, 48 kHz mono, 20 ms frames of 484 bytes. That is about
  **194 kbps per talker** (≈ 24 kB/s). No C libraries or CMake are needed. Screen sharing
  uses hardware H.264 through Windows Media Foundation when the GPU has an encoder, else
  JPEG; the `windows` crate this pulls in makes the first client build take longer.
- **GUI:** egui/eframe with the glow renderer.

## Layout

| Crate | What |
|---|---|
| `crates/vc-proto` | Wire format only: messages, framing, voice datagram layout. |
| `crates/vc-server` | Library plus `vc-server` binary: config, sans-IO `ServerState`, TLS, QUIC net layer. |
| `crates/vc-client` | Library plus `vc-client` binary: DSP, sessions, profile, TOFU, net, audio, pipeline, GUI, auto-update. |
| `crates/vc-release` | `vc-release` tool: release keys, build + sign + publish client updates (Windows and Linux). |
| `crates/vc-mobile` | The iOS core: the client's headless app behind a C bridge that speaks JSON (`include/vc_mobile.h`). |
| `ios/` | The iOS app (SwiftUI), `ios/VoiceChat.xcodeproj`. See [iOS client](#ios-client). |

## Build

```powershell
cargo build --workspace --release
```

### Linux

The client also builds and runs on x86-64 Linux (X11 or Wayland). It needs these
development packages (Ubuntu/Debian names):

```bash
sudo apt-get install -y libasound2-dev libxkbcommon-dev libwayland-dev libx11-dev libgl-dev pkg-config
```

```bash
cargo build --release -p vc-client
```

See [Linux client](#linux-client) for what differs from Windows.

## Run the server

```powershell
cargo run -p vc-server -- --bind 127.0.0.1:7878
```

The server logs its certificate fingerprint at startup. Flags:

| Flag | Meaning |
|---|---|
| `--config <file>` | TOML config (see below). CLI flags override it. Without this flag the server uses `server.toml` from the working directory or, failing that, from the folder holding the executable, if one exists. |
| `--bind <addr>` | Listen address. Default `127.0.0.1:7878`; use `0.0.0.0:7878` for remote clients. |
| `--name <text>` | Server name shown to clients. |
| `--password <text>` | Require this password. |
| `--cert-dir <dir>` | Where `cert.der` / `key.der` live (created on first run). Default `./server-data`. |

Example `server.toml` (every key is optional):

```toml
name = "My Server"
bind = "127.0.0.1:7878"
motd = "Welcome!"
password = "secret"          # omit for an open server
max_users = 64
allow_channel_edit = false   # true lets users rename/delete permanent channels
channels = ["Lobby", "AFK"]  # permanent channels; the first is the default
cert_dir = "./server-data"
hello_timeout_ms = 5000
idle_timeout_ms = 15000
keep_alive_ms = 5000
control_queue = 256          # per-client queue; a client that falls behind is disconnected
max_image_bytes = 4194304    # largest chat image in bytes (1 to 8388608, default 4 MiB)
max_screen_fps = 30          # screen-share frame-rate cap, 0 to 120 (default 0 = no cap)
max_screen_height = 720      # screen-share height cap: 0 (no cap), 480, 720, 1080 or 1440
voice_preset = "balanced"    # competitive (lowest delay) | balanced | smooth (fewest dropouts)
```

Screen-share caps lock every sharer to at most that setting (here 720p at 30 fps). Clients
clamp their capture to them and mark higher choices "(over server limit)" in *Settings*.
The server also drops frames that arrive faster than the fps cap (a two-frame token bucket,
so network jitter never drops an honest sharer). The height cap is enforced by the client only: the server does not parse frames.

Voice presets: the server sends its `voice_preset` to every client in Welcome, and each
client uses that server's tuning for its speakers. The values are frames of 20 ms: the
jitter buffer depth, and the playback backlog the client tolerates before trimming it.

| Preset | jitter / keep | Delay vs balanced |
|---|---|---|
| `competitive` | 2 / 2 | about 20 ms less, up to 40 ms less when a backlog builds; more dropouts on jittery links |
| `balanced` | 3 / 3 | default; same buffer depths as before presets |
| `smooth` | 6 / 4 | about 60 ms more; rides out wifi jitter |

On every preset, including `balanced`, a jitter backlog that stays above the preset's depth
for 500 ms is trimmed back to that depth, so delay that builds from clock drift does not
linger. That trim can cause a short audible skip. A depth change only lowers the delay of a
speaker who is already talking. If the preset raises the depth, the change applies once that
speaker next pauses.

All presets use the same IMA-ADPCM codec at ~194 kbps, so voice fidelity is identical; only
delay and dropout resistance change. An unknown preset name stops the server at startup.
A true-fidelity mode (raw PCM in 10 ms frames) is future work.

Channel rules: config channels are permanent. Any user can create temporary channels (the
creator joins it) and rename or delete them. A temporary channel is deleted when its last
member leaves. The default channel can never be deleted. Runtime channels do not survive a restart.

Ctrl-C shuts the server down cleanly.

## Run the client

```powershell
cargo run -p vc-client
```

| Flag | Meaning |
|---|---|
| `--profile <dir>` | Profile folder for `servers.toml` and `settings.toml`. Default: `%APPDATA%\vc-client`. |
| `--audio cpal\|fake-sine\|fake-silent\|null` | Audio backend. `cpal` (default) uses real devices; if they cannot be opened the app runs without audio and shows a banner. `fake-sine` sends a 440 Hz tone. |
| `--autoconnect <label>` | Connect to this saved server at startup. Can be repeated. |

Two clients on one machine:

```powershell
cargo run -p vc-client -- --profile .\client-a --audio fake-sine
```

```powershell
cargo run -p vc-client -- --profile .\client-b
```

### Using it

- **Servers (left):** *Add server*, then *Connect*. Several servers can be connected at
  once. Playback mixes every connected server. Your microphone goes only to the server
  marked `[talking]`, which is the first one you connected. Use *Talk here* to change it.
  Editing or removing a connected server disconnects it first. Dropped connections
  reconnect on their own. A wrong password, a taken nickname, a protocol mismatch or a
  changed server identity stops the retries and shows an error instead.
- **Channels (middle):** double-click a channel or press *Join*. *New channel*, *Rename*
  and *Delete* act on your current channel. Right-click a user to set their volume
  (0–200 %). The volume is stored per server and nickname.
- **Chat (right):** messages go to your current channel. Enter sends. Timestamps are in UTC.
  Paste an image (Ctrl+V) or drop a PNG or JPEG file on the window to send it to the
  channel. Images larger than 2560 px or the server's `max_image_bytes` are scaled down and
  re-encoded first. Click a thumbnail to open it in its own window (resizable, several
  at once; the window's close button or *Close image* closes it). The client keeps the last 24 images per
  server in memory; older ones show as *[image expired]*. Nothing is saved to disk.
- **Screen share:** *Share screen* in the bottom bar shares your primary monitor with your
  current channel, and the channel list marks you *[sharing]*. Watching is opt-in: others
  see a row "*nick* is sharing their screen" with a *Watch* button at the top of the middle
  panel. Watched streams open in a separate *Streams* window laid out as a grid (1, 2×1,
  2×2, 3×2, 3×3, …), with streams from every connected server; *Stop watching* removes
  one, and closing the window stops watching all of them. Frames are only captured and
  sent while someone watches, and the sharer sees "*N* watching" under the banner.
  Click a sharer's LIVE badge in the channel list to start watching (or bring the
  Streams window forward when popped out).
  *Settings* holds the frame rate (1–120 fps), resolution (480p, 720p, 1080p, 1440p or
  native up to 2560 px; each means "fits in") and JPEG quality (20–95), defaulting to
  10 fps, 720p and quality 60. Changes apply on *Save*, also while sharing. A frame is at
  most 1 MiB; larger ones automatically fall back to a lower quality and size. These are
  upper limits: the real frame rate depends on CPU and bandwidth. Screen capture uses GDI,
  which bounds the rate to about 30 fps at 1440p (on the dev box; higher on smaller
  screens), so 120 fps is a ceiling, not a promise. Sharing stops when you
  press *Stop sharing*, change channel or disconnect, and watching does not carry over to
  a new share.
  *Test max* in *Settings* measures this machine's **local encode limit**: it captures and
  encodes the screen for about 0.8 s at each of 1440p, 1080p, 720p and 480p (up to the
  screen's own height), takes 85 % of each rate as sustainable, and picks the highest
  resolution that holds 30 fps, at the largest offered frame rate it sustains. The result
  is applied to the combos and saved at once; change the combos and *Save* to override it.
  It does not measure the network: one frame is in flight per connection at a time, so
  over the internet the frame rate is limited to about 1 / round-trip time whatever is set.
- **Hardware encoding:** when the GPU has a Media Foundation H.264 encoder (NVIDIA NVENC,
  Intel Quick Sync, AMD AMF) and every watcher's client can decode H.264, frames are sent
  as all-intra H.264 (each frame decodes on its own). Otherwise, for any frame too large,
  and for the rest of a share after a hardware error, frames are JPEG. *Settings* shows
  the encoder in use and has *Use hardware encoding* to force JPEG. Received H.264 goes to
  the system's native decoder; the server only checks the start code, which is not
  validation. Non-Windows clients always get JPEG.
- **Stream sound:** the sharer's banner has *Stream sound: Off/On* (off by default, and
  remembered). When on, your system audio is sent to your watchers, excluding this app's
  own output, so watchers never hear their own voices echoed. It is sent even while your
  mic is muted, since it is not your voice. It needs Windows 10 version 2004 or later;
  otherwise it turns itself off with "Stream sound unavailable". Each watcher can *Mute*
  a stream's sound in its tile, for themselves only (the sound is still received).
  Each watcher can set a stream's sound volume (0-200 %, remembered per server and nick)
  from the user menu or by right-clicking the tile's speaker button; Mute still silences it.
- **Bottom bar:** *Mute*/*Deafen* apply to every server; deafen also mutes. *Input mode*
  is *Voice activation*, *Push to talk* or *Always on*. The bar also has the input level
  meter and *Settings*, which holds devices, the VAD threshold, output volume, the PTT key,
  your nickname and the screen-share quality.
- **Push to talk works only while one of the app's windows (the main window, the Streams
  window or an image window) has focus and no text field is focused.** There is no
  global hotkey. Paste and drop work in the main window only.
- **Protocol version 4:** screen-share caps, H.264 frames and stream sound need server
  support, so the server and every client must be updated together; mixed versions
  refuse each other with a protocol mismatch error.
- **Mute and deafen apply to all connected servers at once.** There is one microphone
  and one output mix.
- **The talk target stays where you left it.** It moves to another server only when the
  talk server disconnects, and it never moves back on its own. Use *Talk here* to choose it.
- **Server passwords are stored in plain text** in `<profile>/servers.toml`, so do not
  reuse a valuable password for a voice server.
- **A Settings nickname change applies on the next manual *Connect*** (an automatic
  reconnect keeps the current nick). It updates only the servers that used the old
  default nick; a nick set in a server's own Edit dialog is kept.
- **Server identity:** the first connect pins the server certificate's fingerprint. If it
  changes later, a dialog shows the new fingerprint, and you choose *Trust new identity*
  or *Cancel*.

## iOS client

An iPhone and iPad app in `ios/`. It runs the same Rust client as the desktop
(network, audio pipeline, sessions; built from `crates/vc-mobile` as a static library),
and SwiftUI draws it. Everything the desktop client does works, except **sharing**: the
app can watch other people's streams but cannot share its screen or stream sound.

### Build

Needs Xcode 26 (iOS 17 or later) and rustup with the iOS targets. With Homebrew:

```bash
brew install rustup
export PATH="/opt/homebrew/opt/rustup/bin:$HOME/.cargo/bin:$PATH"
rustup toolchain install stable --profile minimal -c clippy,rustfmt
rustup target add aarch64-apple-ios aarch64-apple-ios-sim
```

Then open the project and run the `VoiceChat` scheme:

```bash
open ios/VoiceChat.xcodeproj
```

Its first build phase, "Build Rust core", runs `ios/build-rust.sh`, which builds
`vc-mobile` with cargo for the simulator or device and links it from `target/`. From the
command line:

```bash
xcodebuild -project ios/VoiceChat.xcodeproj -scheme VoiceChat -destination 'platform=iOS Simulator,name=iPhone 17 Pro' build test
```

The unit tests decode the golden files in `crates/vc-mobile/tests/golden`, which
`cargo test -p vc-mobile` writes and checks (`VC_BLESS=1` rewrites them), so the Swift
and Rust sides of the bridge cannot drift apart. The app ships on the App Store as
**Rustle Voice** (bundle ID `xyz.rc-wreding.rustlevoice`, team `25PK93UTQ9`, automatic
signing). To upload a new build, raise `CURRENT_PROJECT_VERSION` (and
`MARKETING_VERSION` for a new release), then:

```bash
xcodebuild -project ios/VoiceChat.xcodeproj -scheme VoiceChat -configuration Release -destination 'generic/platform=iOS' -archivePath ios/build/RustleVoice.xcarchive -allowProvisioningUpdates archive
```

```bash
xcodebuild -exportArchive -archivePath ios/build/RustleVoice.xcarchive -exportOptionsPlist ios/ExportOptions.plist -exportPath ios/build/upload -allowProvisioningUpdates
```

### Launch settings

These are read from the app's user defaults, so they can also be given as launch
arguments:

- `-vcAudio external|engine-synth|fake-sine|none`. `external` is the real microphone with
  echo cancellation (the default on a phone). `engine-synth` never opens the microphone:
  it feeds a 440 Hz tone through the audio engine instead (the default in the
  simulator, so no microphone prompt appears). `fake-sine` is the core's own tone without
  an audio engine, and `none` has no audio.
- `-vcProfile <dir>`: profile folder (default Application Support/vc-client, with the
  same `servers.toml` and `settings.toml` as the desktop).
- `-vcSeed "label=host:port=nick"`: add and connect that server on launch if no server
  has that label.

```bash
xcrun simctl launch booted xyz.rc-wreding.rustlevoice -vcAudio engine-synth -vcSeed "Local=127.0.0.1:7878=ios"
```

### A test peer

`fake_peer` is a scripted desktop client for testing by hand. It talks a 440 Hz tone and
can share the synthetic test screen, send stream sound, chat and images. It prints what
it hears and sees (`joined`, `hearing`, `chat`, `image from`, `watchers`, ...):

```bash
cargo run -p vc-client --example fake_peer -- --addr 127.0.0.1:7878 --nick bob --share --stream-sound --say hi --image pic.png
```

### Differences from the desktop

- No screen sharing, stream sound, "Test max", push-to-talk key, shortcuts, file drop or
  auto-update. Push-to-talk is a hold-to-talk button in the voice bar.
- Audio devices are the system's routes (speaker, Bluetooth, AirPlay picker). Echo
  cancellation is on by default; without it, sound plays from the earpiece.
- Voice keeps running in the background and with the screen locked.
- Watching uses JPEG only (the app does not decode H.264). The server falls back to JPEG
  for a sharer whenever any watcher cannot decode H.264, so one iOS watcher switches that
  share to JPEG for everyone watching it.
- Chat shows the newest 200 lines of a channel.
- Passwords are stored in `servers.toml` in the app's sandbox, as on the desktop.

## Linux client

Released together with the Windows client, as one x86-64 binary,
`vc-client-<version>-x86_64-linux`, from the same download page. It needs glibc 2.39 or
newer (it is built on Ubuntu 24.04), ALSA (`libasound2`), xkbcommon, OpenGL (Mesa) and,
for updates, `curl`.

### Install

```bash
mkdir -p ~/.local/opt/voicechat
mv ~/Downloads/vc-client-<version>-x86_64-linux ~/.local/opt/voicechat/vc-client
chmod +x ~/.local/opt/voicechat/vc-client
~/.local/opt/voicechat/vc-client
```

Rename the file to `vc-client`. The updater keeps its swap files next to it
(`vc-client.old`, `vc-client.<pid>.new`) and its state in `~/.local/share/vc-client/update/`.
The profile is in `~/.config/vc-client/`.

### Differences on Linux

- No screen-share sending yet: the Share screen button is disabled, Ctrl+Shift+S does
  nothing and the shortcut is not listed. Watching other people's shares works.
- No stream sound and no H.264. Watching uses JPEG only. The server falls back to JPEG for
  a sharer whenever any watcher cannot decode H.264, so **one Linux watcher switches that
  share to JPEG for everyone watching it** (the same as an iOS watcher).
- Updates are fetched with the system `curl` (`curl` must be installed; without it the
  check fails and is retried). It uses the system CA store and curl's proxy environment
  variables (`https_proxy`, `no_proxy`, ...); `~/.curlrc` is ignored.

## Auto-updates

Release builds of the Windows and Linux clients update themselves from
`https://voicechat.owl.gay`, silently. A few seconds after start, and every minute after that, the client checks for a
new version. When it finds one it downloads it in the background, checks it, and puts it
in place. The running client is never restarted: the next launch is the new version. The
current state is shown under Settings → Profile → About.

### Trust model

The update host is **not** trusted. A hijacked host, DNS record or TLS certificate can at
worst hold updates back; it cannot make a client run its code.

- Each release is described by `latest.signed`: a manifest (target, version, exact size,
  SHA-256, issue and expiry time) signed with an **offline Ed25519 release key**. Each
  target has its own: Windows at `/<channel>/latest.signed`, Linux at
  `/<channel>/linux/latest.signed`. A client only accepts a manifest for its own target. The public keys
  are compiled into the exe from `crates/vc-client/update-keys.txt`.
- The client checks the signature before it reads the manifest, then the exe's size and
  SHA-256. It then runs the new exe once with `--update-selftest`, and only after all of that
  does it touch the installed exe.
- Downgrades and replays are refused: the version must be higher than the installed one,
  and a manifest older than the newest one already seen is ignored. Manifests expire after
  180 days, so a frozen host cannot hide new releases for ever.
- The host name is fixed in the code. Redirects are refused, only status 200 is accepted,
  and every download is capped at the size the manifest names. HTTPS (TLS 1.2+) uses the
  Windows certificate store and proxy settings on Windows, and `curl` with the system CA
  store on Linux.
- A new version that does not stay up for 30 seconds in three launches in a row is rolled
  back to the previous exe and never fetched again.

### One-time setup

`update-keys.txt` ships empty, and while it lists no key the updater is off. To turn it on:

1. Create two keys, a primary and a backup, each on its own offline medium (for example
   two USB sticks). Never put a key in the repository or on the update host. The tool refuses
   to write one inside the repository.

   ```powershell
   cargo run -p vc-release -- keygen --out E:\voicechat-release-primary.pkcs8
   ```

   ```powershell
   cargo run -p vc-release -- keygen --out F:\voicechat-release-backup.pkcs8
   ```

2. Paste both printed public keys into `crates/vc-client/update-keys.txt` and commit. If the
   primary key is lost or leaked, publish with the backup, and ship a release whose
   `update-keys.txt` drops the bad key.

### Linux build setup (once, on the release PC)

`publish` builds the Linux client in WSL on the same Windows PC, so the release key never
leaves it. Set up the WSL distribution once (default `Ubuntu`, 24.04; another one can be
picked with the `VC_WSL_DISTRO` environment variable):

```powershell
wsl -d Ubuntu -e bash -c 'sudo apt-get update && sudo apt-get install -y build-essential curl libasound2-dev libxkbcommon-dev libwayland-dev libx11-dev libgl-dev pkg-config'
```

```powershell
wsl -d Ubuntu -e bash -c 'curl --proto =https --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain 1.95 -c clippy,rustfmt'
```

The distro's glibc sets the oldest Linux the binary runs on (2.39 for Ubuntu 24.04). The
download page and this README say so; change both if you build on another distro.

### Publishing a release

Every release publishes Windows and Linux together, from one command.

1. Bump `version` in `[workspace.package]` in the top-level `Cargo.toml`.
2. Build, self-test, sign and lay out the upload folder:

   ```powershell
   cargo run -p vc-release -- publish --key E:\voicechat-release-primary.pkcs8 --out dist
   ```

   This builds `vc-client` with `VC_UPDATE_CHANNEL=stable` into its own folder,
   `target/channel-stable/` (`target/channel-<channel>/` in general), so your everyday
   `target/release` exe is untouched. It then builds the Linux client in WSL (into
   `~/.cache/vc-release/channel-<channel>/` inside WSL, copied to
   `target/channel-<channel>-linux/vc-client`) and self-tests it there. Both builds use
   `--locked`. A plain `cargo build` has no channel, and its binary never updates itself.

   The tool checks both targets before it builds: it refuses a version that is not higher
   than the Windows or the Linux one already in `dist`. Nothing is written unless both
   binaries built and passed their self-test. The manifests are written last, Linux first
   and Windows last. `dist/index.html`, the public download page, always describes the
   stable release: only a `stable` publish writes it.

   `--windows-only` publishes Windows without Linux, with a loud warning. Use it only in an
   emergency (WSL broken and a Windows fix cannot wait) or to recover from a publish that
   failed part way; Linux clients then stay on their version until the next full publish,
   and the page keeps showing the Linux release already in `dist`.
3. Upload `dist/` to the host in the order the tool prints:
   1. `stable/vc-client-<version>-x86_64.exe`
   2. `stable/linux/vc-client-<version>-x86_64-linux`
   3. `index.html`
   4. `stable/linux/latest.signed`
   5. **`stable/latest.signed` last**
4. Check the live files: `cargo run -p vc-release -- verify`. It checks the manifest and
   binary of every target; for `stable` and `test` a missing Linux release is an error.

#### Test the update itself before every stable release

The health check only proves that the new version starts. A release whose window opens but
whose updater is broken passes it, and then strands every client on that version for good:
rollback cannot fix that, because the broken release is the one that would have to fetch the
fix. So before each `stable` publish, check that the previous release really updates to the
new one:

0. Before publishing anything, set up the **previous** version:
   - Create a writable test folder, for example `%LOCALAPPDATA%\Programs\Voicechat-test\`.
   - Copy the previous version's test-channel exe there as `vc-client.exe`. Take it from
     `dist/test/vc-client-<previous>-x86_64.exe` (left there by the previous test publish),
     or from that release's saved copy.
   - If neither exists (first run, fresh clone), check out the previous version's commit, run
     `publish --channel test` for it once, and copy that exe.
   - Never use `target/channel-test/release/vc-client.exe` as the previous exe: the next
     publish overwrites it.
1. Publish the new version to the test channel. It is served from `/test/` on the host:

   ```powershell
   cargo run -p vc-release -- publish --key E:\voicechat-release-primary.pkcs8 --out dist --channel test
   ```

   Upload `dist/test/` (exe first, `latest.signed` last). The test channel never touches
   `dist/index.html`.
2. Check the copied exe from step 0 still is the previous build: run
   `vc-client.exe --update-selftest x` in the test folder. It must print the previous version
   and `test`. Then start it.
3. Within about a minute `vc-client.old.exe` should appear next to it, and Settings → Profile
   should say the update is ready. Close it and start it again: it must be the new version.
4. Do the same for Linux with the previous Linux test-channel build
   (`dist/test/linux/vc-client-<previous>-x86_64-linux`), copied to a writable folder as
   `vc-client` with `chmod +x`: `./vc-client --update-selftest x` must print the previous
   version and `test`; after a minute `vc-client.old` appears beside it, and the next start
   is the new version. This test starts with the second Linux release: the first Linux
   release has no previous Linux version to update from.
5. Only then publish to `stable`.

Re-sign before 180 days pass, even with no new release (`verify` warns when fewer than 30
days are left). `resign` re-signs every target published in the channel. Then upload the
new `linux/latest.signed` and `latest.signed` (Windows last):

```powershell
cargo run -p vc-release -- resign --key E:\voicechat-release-primary.pkcs8 --out dist
```

### Hosting rules

- Static files only, over HTTPS, with no redirects (the client does not follow them).
- Serve every `latest.signed` (including `<channel>/linux/latest.signed`) with
  `Cache-Control: no-cache`, and the versioned binaries (`vc-client-<version>-x86_64.exe`
  and the extension-less `vc-client-<version>-x86_64-linux`) with
  `Cache-Control: public, max-age=31536000, immutable`. Match by file name, not by
  extension.
- Never overwrite a published binary. Every release gets a new file name (the tool refuses
  to replace one in `dist` with different contents).

### Installing and opting out

- Install the client in a folder your user can write to, for example
  `%LOCALAPPDATA%\Programs\Voicechat`. In a folder it cannot write to (such as
  `Program Files`), the updater turns itself off and never asks for admin rights.
- The first install is manual: download it from `https://voicechat.owl.gay/`. Existing 0.1.0
  installs have no updater and need this one manual install.
- The exe is not Authenticode-signed yet, so Windows SmartScreen may warn on the first
  download. Updates that the client installs itself are not affected.
- Opt out with `vc-client --no-update`, or by setting the `VC_NO_UPDATE` environment variable.
- Manual recovery: while `vc-client.old.exe` is next to the exe, close the client, delete
  `vc-client.exe`, and rename `vc-client.old.exe` to `vc-client.exe`. On Linux the same
  with `vc-client.old` and `vc-client`.

## Tests

```powershell
cargo test --workspace
```

```powershell
cargo clippy --workspace --all-targets -- -D warnings
```

```powershell
cargo fmt --all -- --check
```

Real-device audio tests are ignored by default:

```powershell
cargo test -p vc-client --test cpal_devices -- --ignored
```

Hardware video tests (need a GPU with an H.264 encoder) are ignored too:

```powershell
cargo test -p vc-client --lib -- --ignored --nocapture
```

The test suite covers:

- **Protocol:** framing and every message variant.
- **Server state machine:** tested sans-IO (handshake, channel rules, chat scope, voice
  forwarding, mute and deafen, image scope and limits, screen-share state).
- **Server over real QUIC:** idle timeout through a black-hole relay, slow-consumer kick,
  shutdown, image streams (refused before Welcome, size limit, a stalled image never
  blocks chat).
- **Client DSP:** ADPCM SNR, jitter buffer, mixer, VAD, resampler.
- **Client session and app model:** sessions, the app model, TOFU (including a pinned
  certificate presented with the wrong key), and end-to-end audio between in-process clients
  on fake devices.
- **GUI:** headless egui_kittest tests of every widget against real in-process servers.
- **iOS core:** the bridge's commands and views against real in-process servers
  (including watching a fake share), and golden JSON files shared with the Swift tests.

## License

Copyright (C) 2026 Amadeus Wreding

This program is free software: you can redistribute it and/or modify it under the terms of
the GNU General Public License as published by the Free Software Foundation, either version 3
of the License, or (at your option) any later version. See [LICENSE](LICENSE) for the full
text.
