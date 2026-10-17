# Zed live harness (`zh`)

Lets an agent that only has a shell and an image viewer run a **real Zed GUI** headlessly,
send it keyboard/mouse input, read its clipboard and logs, and take screenshots, without
touching the user's desktop session or their Zed config/data.

Stack: **Xvfb** (private virtual X display) + **lavapipe** (Mesa's CPU Vulkan driver) +
**xdotool** (input) + **ImageMagick `import`** (screenshots) + **xclip** (clipboard) + a
**private dbus-daemon**. Tools come from nixpkgs and are pinned with GC roots on first use.

## Quick start

```sh
ZH=/home/drew/personal/zed/script/live-harness/zh

$ZH start                      # installed Zed (nixpkgs zed-editor) on a demo repo
$ZH start --fork               # the fork: <repo>/target/release-fast/zed
$ZH start --fork --workdir /path/to/repo --size 1600x1000 --fresh

$ZH keys ctrl+shift+p          # command palette
$ZH type "git: diff"
$ZH keys Return
$ZH shot                       # prints the PNG path; open it with the Read tool
$ZH keys '] c' '] c' g g G     # vim keys, one keypress per token
$ZH clip                       # clipboard contents (e.g. after a yank / copy action)
$ZH logs --errors              # Zed.log of this session, errors/warnings only
$ZH stop
```

`zh help` lists every subcommand. Summary:

| Command | What it does |
| --- | --- |
| `start [--binary PATH \| --fork] [--workdir DIR] [--size WxH] [--fresh] [--settings F] [--keymap F]` | Start Xvfb + dbus + Zed opening DIR. Idempotent: if already running with the same options it just prints status; with different options it refuses (use `restart`). Returns once the first frame is rendered. |
| `restart [start options]` | `stop` then `start`. |
| `stop [--clean]` | Kill Zed (whole process group), dbus, Xvfb; remove the `$XDG_RUNTIME_DIR/zh` link. `--clean` also deletes the run dir (isolated HOME, logs, screenshots). |
| `status` | Display, PIDs, window id, binary + its mtime, workdir. Exit 1 if not running. |
| `keys KEY...` | One xdotool keypress per whitespace-separated token. Chords: `ctrl+shift+p`, `alt+h`, `shift+Tab`. Punctuation is mapped for you (`]`, `[`, `:`, `/`, `ctrl+]`…). Aliases: `esc enter tab bs del up down left right pgup pgdn home end`. Uppercase letters (`G`) send shift automatically. Use `space` for a space key. |
| `type TEXT` | Type literal text (25 ms/char). Newlines become Return. |
| `click X Y [BTN]`, `dblclick X Y`, `rclick X Y`, `move X Y`, `scroll up\|down [N]`, `park` | Mouse. Coordinates are screen pixels = Zed window pixels (window is at 0,0 and fills the screen). `park` moves the pointer to the bottom-left corner so hover effects don't pollute screenshots. |
| `shot [OUT.png] [--now] [--crop WxH+X+Y] [--scale PCT]` | Screenshot of the whole virtual screen. Waits `ZH_SHOT_DELAY_MS` (400 ms) first so the last input is rendered, unless `--now`. Default output: `<state>/run/shots/shot-HHMMSS-mmm.png`. `--crop` is handy to zoom into the status bar or a gutter. |
| `clip [--primary] [--targets] [--target T]` | Print the CLIPBOARD (or PRIMARY) selection of the virtual display, i.e. what the test Zed copied. |
| `clip-set TEXT` | Put TEXT on the virtual display's clipboard (to test pasting). |
| `logs [N] [--errors] [--all] [--path]` | Tail this session's part of the isolated `Zed.log` (default 60 lines). Shows Zed's stdout too if the process died. |
| `wait-log REGEX [TIMEOUT_S]` | Block until this session's log matches (e.g. a `log::info!` you added). |
| `open PATH[:LINE[:COL]]` | Open a file in the running instance through the Zed CLI (relative paths are relative to the workdir). Only works when a CLI binary sits next to the app (nixpkgs `zeditor`, or `target/<profile>/cli`); the fork build usually has no `cli`, so use the file finder: `keys ctrl+p; type NAME; keys Return`. |
| `cli [-C DIR] ARGS...` | Run the CLI next to the app (`target/<profile>/cli` for the fork: build `-p zed -p cli` before `start`) against the running instance, with its default open behaviour (no `--existing`) and DIR as the working directory (default the workdir), e.g. `zh cli --review uncommitted`, `zh cli -C DIR --wait --review branch`. Its stdout, stderr and exit status are the CLI's. The isolated environment makes it reach only the harness's instance; never run the fork's `cli` directly. |
| `mkrepo DIR` | Create a scratch git repo with a commit plus uncommitted modifications, an addition, a deletion and an untracked file (what `start` uses when no `--workdir` is given). |
| `env` | Print `export DISPLAY=…` etc. to drive the display directly (`eval "$(zh env \| grep ^export)"`, then `$XDOTOOL`, `$XCLIP`, `$IMPORT`). |
| `path run\|home\|config\|data\|log\|shots` | Print isolated paths. `zh path config` is the live settings dir: editing `settings.json` there applies immediately (Zed watches it). |
| `xdo ARGS...` | Run xdotool against the private display. |

## Typical agent loop

1. `zh start --fork --workdir <repo with changes>` (≈1 s).
2. Act: `zh keys …` / `zh type …` / `zh click X Y`.
3. Look: `zh shot` and Read the PNG. Use `--crop` to zoom in on small text.
4. Check: `zh logs --errors`, `zh clip`, `zh status` (a dead Zed means a crash — `zh logs` shows the panic).
5. `zh stop` when done (do not leave it running; see CPU limits).

Tips:

- Vim multi-key sequences (`] f`, `g g`, `ctrl+w h`) are sent as separate keypresses 60 ms
  apart (`ZH_KEY_DELAY_MS`), well within Zed's multi-keystroke timeout.
- The editor starts in vim **normal** mode. `zh type` in normal mode executes vim commands;
  press `i` first to insert text, `Escape` to leave insert mode.
- After `start`, focus is in the workspace (project panel or editor). If a screenshot
  shows a modal/picker you didn't expect, `zh keys Escape`.
- Rebuilt the fork? `zh restart --fork`. `zh status` shows the binary mtime so you can
  confirm you're running the new build.
- `--fresh` wipes the isolated HOME (Zed DB, logs, extensions) first. Without it, Zed's DB
  persists across restarts (useful to test persistence; the workspace layout of a previous
  run is *not* restored because `restore_on_startup` is `none`).
- Extra env for the test Zed: `ZH_EXTRA_ENV="RUST_LOG=info,code_review=debug RUST_BACKTRACE=1" zh start --fork`.

## Isolation guarantees

The user's own session and Zed are never touched:

- **Display**: Zed runs on a private Xvfb display `:90`–`:140` (first free one; checks lock
  files, filesystem sockets *and* abstract sockets). Nothing appears on the real desktop, and
  xdotool/xclip/import only ever talk to that display. Never use `Xvfb -displayfd`: on this
  machine Xwayland's `:0` only has a filesystem socket, so `-displayfd` happily picks `:0`
  and grabs the abstract socket that the user's X clients connect to first.
- **Environment**: Zed is launched with `env -i` and an allowlist (PATH, locale, fonts,
  TZ). `WAYLAND_DISPLAY`, `HYPRLAND_INSTANCE_SIGNATURE`, `SSH_AUTH_SOCK`, the user's
  `DBUS_SESSION_BUS_ADDRESS` etc. are dropped, so it cannot open a Wayland window on the
  real compositor or reach the user's agent/keyring/portals.
- **Config/data**: `HOME`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_STATE_HOME`,
  `XDG_CACHE_HOME`, `XDG_RUNTIME_DIR` all point into the harness state dir
  (`${XDG_STATE_HOME:-~/.local/state}/zed-live-harness/run/home`). `~/.config/zed` and
  `~/.local/share/zed` are never read or written. The only link to the real home is a
  symlink `home/.nix-profile -> ~/.nix-profile` (read-only use) so Zed's login-shell PATH
  still finds `git`.
- **Single-instance socket**: Zed's CLI socket lives in its data dir
  (`<isolated>/.local/share/zed/zed-<channel>.sock`), so the harness instance and the
  user's Zed can never talk to each other, whatever their release channels.
- **DBus**: a private `dbus-daemon` with no service directories: nothing (gnome-keyring,
  xdg-desktop-portal, notification daemons) can be activated, so Zed cannot read the
  user's stored credentials and never signs in to zed.dev (expect harmless
  `org.freedesktop.secrets was not provided` errors in the log).
- **Process cleanup**: Xvfb, dbus and Zed each run in their own session (`setsid`); `stop`
  kills whole process groups (crash handler, language servers, shell probes), and only
  after verifying each PID really belongs to this harness (stale PIDs are never signalled).
- **Network/noise**: the harness settings disable telemetry, auto-update, all AI features,
  extension auto-install, language servers, the worktree-trust prompt and sign-in UI.

Residual exposure: the Xvfb display accepts any local client (no xauth cookie), and Zed
can still reach the network if a feature under test does so.

## CPU limits (shared machine)

Compute is shared with cargo builds and other agents, so the defaults are conservative:

| Variable | Default | Effect |
| --- | --- | --- |
| `ZH_CPUS` | last 2 CPUs (`14-15` here) | `taskset` CPU list for Xvfb and Zed. `ZH_CPUS=` (empty) disables pinning. |
| `ZH_NICE` | `10` | `nice` level for Xvfb, dbus and Zed. |
| `ZH_LP_THREADS` | `2` | lavapipe rasterizer threads (`LP_NUM_THREADS`). |

Idle Zed uses ~0–5% CPU; redraws burst onto the pinned cores only. Run **one instance at a
time** (the harness only supports one per state dir anyway) and `zh stop` when finished.
Never start a cargo build from a harness session.

## Other configuration

| Variable | Default | |
| --- | --- | --- |
| `ZH_BINARY` | `zeditor` from PATH (resolved to the app binary) | default for `start` without `--binary/--fork` |
| `ZH_FORK_BINARY` | `<repo>/target/release-fast/zed` | what `--fork` runs |
| `ZH_WORKDIR` | `<state>/demo-repo` (created on demand) | default `--workdir` |
| `ZH_SIZE` | `1600x1000` | screen = window size (`ZED_WINDOW_SIZE`) |
| `ZH_KEY_DELAY_MS` / `ZH_TYPE_DELAY_MS` / `ZH_SHOT_DELAY_MS` | `60` / `25` / `400` | input pacing, screenshot settle time |
| `ZH_START_TIMEOUT_S` | `90` | how long `start` waits for the window + first frame |
| `ZH_STATE_DIR` | `${XDG_STATE_HOME:-~/.local/state}/zed-live-harness` | all runtime state |
| `ZH_LINK` | `$XDG_RUNTIME_DIR/zh` (`/tmp/zh-$UID` without a runtime dir) | short symlink to `<state>/run` (see gotchas); only followed when you own it |
| `ZH_VK_ICD` | `/run/opengl-driver/share/vulkan/icd.d/lvp_icd.x86_64.json` | lavapipe ICD |
| `ZH_SHELL` | caller's `$SHELL` | login shell Zed uses to load its environment |
| `ZH_EXTRA_ENV` | – | space-separated `VAR=value` pairs added to Zed's environment |

`config/settings.json` and `config/keymap.json` are copied into the isolated config dir on
every `start` (or pass `--settings/--keymap`). They mirror the behaviour-relevant parts of
Drew's real config: `vim_mode`, `base_keymap: VSCode`, vim relative line numbers / yank
highlight, `cursor_blink: false`, autosave after 500 ms, panel docks (project/git/outline
left), font family/sizes, per-language soft wrap / tab sizes, and his custom key bindings
(minus the lazygit task). Theme is One Dark (his Catppuccin Mocha is an extension and not
installed offline). `diff_view_style` is left at Zed's default (`split`), as in his config.

## Timings (measured on this machine)

| Operation | Time |
| --- | --- |
| `zh start` (window + first frame), installed Zed or fork | ~0.7–1.3 s; ~3.5 s under heavy load |
| idle CPU of the test Zed | ~0.3% (redraws burst onto the 2 pinned cores) |
| first `zh start` ever | + a few seconds for `nix build` of xdotool/xclip/imagemagick/xdpyinfo |
| `zh shot --now` | ~150 ms (plus 400 ms settle delay without `--now`) |
| `zh keys` | ~60 ms per token + ~10 ms process overhead |
| `zh open` via CLI | ~30 ms + render |
| `zh stop` | < 1 s |

## Gotchas

- **Socket path length**: Unix socket paths must be < 108 bytes. Zed puts its CLI socket in
  its data dir, so the isolated Zed only sees the state dir through the short symlink
  `$XDG_RUNTIME_DIR/zh`. Paths in Zed's UI/logs therefore show either the real path (worktrees are
  canonicalized) or `/run/user/1000/zh/...`.
- **Clipboard targets**: Zed on X11 offers `UTF8_STRING`/`text/plain;charset=utf-8` but not
  `STRING`, which plain `xclip -o` asks for ("target STRING not available"). `zh clip`
  requests `UTF8_STRING`. The clipboard only exists while the owning app runs: read it before
  `zh stop`. Vim yanks go to the system clipboard (Zed's default `use_system_clipboard`).
- **No window manager**: there are no server-side decorations and no WM focus handling.
  The harness focuses Zed's window explicitly before every input command; windows Zed opens
  later are found by PID (`zh focus`).
- **Mouse**: Zed logs `Found no xinput mouse pointers` on Xvfb; clicks and wheel still work
  (verified on the project panel).
- **Rendering is lavapipe only**: Xvfb has no DRI3, so the AMD GPU's Vulkan driver fails with
  "no compatible surface formats" and wgpu falls back to llvmpipe anyway.
  `ZED_ALLOW_EMULATED_GPU=1` suppresses Zed's "software GPU" warning dialog.
- **X11, not Wayland**: Drew's desktop is Hyprland/Wayland, but the harness drives Zed's X11
  backend. Keymap/keystroke handling is shared, but Wayland-specific behaviour (IME,
  wl clipboard, fractional scaling) is not covered. Running a second, detached Hyprland is
  not viable: Hyprland 0.56 has no headless-only mode, so it would either nest (open a
  window on the real desktop) or try to take the seat/DRM/input devices of the live session;
  and driving the *live* Hyprland (a headless output + `wtype`/`hyprctl`) would inject input
  into the user's session.
- **Stale fork binary**: `target/release-fast/zed` may be mid-rebuild; check
  `zh status` → `binary mtime` (nix store binaries always show 1969), and `restart` after a
  rebuild. Starting it while cargo is still linking it can fail; just retry.
- **Inline git blame** (Zed default) appears on the cursor line after a short delay and
  shows up in screenshots; move the cursor or disable `git.inline_blame` via
  `zh path config` if it gets in the way.
