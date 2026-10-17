# Installing the fork as your only Zed (NixOS)

This guide replaces nixpkgs' `zed-editor` with Drew's fork (`drew-council/zed`, branch
`code-review`), built by the fork's own Nix flake as a **stable-channel** app (DESIGN.md §2.1).
Nothing here has been applied to `~/nixconf` yet: these are the edits to make.

> **Not installable yet.** You need two things first:
> 1. `code-review` must be pushed to `origin`. It only exists locally so far.
> 2. `code-review` must be rebased onto a **stable** tag. It is on `v1.25.0-pre`, so
>    `crates/zed/RELEASE_CHANNEL` says `preview`, and the overlay below refuses to evaluate. The
>    first 1.25 stable may be `v1.25.1` or later rather than `v1.25.0` (1.23 and 1.24 first shipped
>    stable as `v1.23.1` and `v1.24.2`).
>
> The rebase skill (`.claude/skills/rebase-code-review`) does both steps; the push needs your
> confirmation.

## What the fork's flake builds

| | |
|---|---|
| Attribute | `packages.<system>.default` (`nix/modules/packages.nix`). `packages.debug` is the `dev` profile. `overlays.default` sets `zed-editor` but builds it with *your* nixpkgs (see "Why no `follows`") |
| Build | `nix/build.nix` through `nix/toolchain.nix`: a crane build using the Rust from `rust-toolchain.toml` (rust-overlay), profile `release` (thin LTO), `-p zed -p cli --locked --features=gpui_platform/runtime_shaders`, tests off |
| Channel | `releaseChannel` argument (a fork addition, DESIGN §3.10): `"nightly"` by default, like upstream, or `"stable"`/`"preview"` (Linux only). It sets `crates/zed/RELEASE_CHANNEL`, the version suffix, the icons and the desktop entry. Pick it with `.override { releaseChannel = "stable"; }` |
| Linux outputs | `bin/zed` (the CLI), `bin/zeditor` (symlink to it), `libexec/zed-editor` (the app, wrapped with nodejs 22 on `PATH`), icons, one desktop entry (`dev.zed.Zed.desktop` for stable). `meta.mainProgram = "zed"` |
| Version | `<crate version>+<sha7>` for stable (`-nightly`/`-preview` before the `+` otherwise). `ZED_COMMIT_SHA` comes from the input's `self.rev` and is empty for a dirty `git+file` tree |

nixpkgs' package ships only `bin/zeditor`. The fork's ships both `zed` and `zeditor`.

### Why the channel matters

Upstream's flake always builds the nightly channel. The fork's install uses `releaseChannel =
"stable"` so the fork takes over your existing Zed:

| | stable (`releaseChannel = "stable"`; what you run today, nixpkgs Zed 1.23.2) | nightly (`packages.default` as-is) |
|---|---|---|
| Database | `~/.local/share/zed/db/0-stable` (your history, workspaces, threads) | `db/0-nightly`: starts empty |
| CLI socket | `~/.local/share/zed/zed-stable.sock` | `zed-nightly.sock` |
| Wayland app id / desktop entry | `dev.zed.Zed` / `dev.zed.Zed.desktop` (what `home/xdg-mime.nix` uses) | `dev.zed.Zed-Nightly` |
| SSH remote server | downloads the exact matching version | downloads the latest *nightly*, likely incompatible |

Settings and keymap (`~/.config/zed/`) are shared by every channel either way.

## Wiring it into `~/nixconf`

Your system config is the flake at `~/nixconf` (`github:drew-council/nixconf`). `/etc/nixos`
appears to hold only the March 2025 installer files.

### 1. `flake.nix`: add the input

```nix
inputs = {
  # …
  zed-fork.url = "github:drew-council/zed/code-review";
};
```

To build a local checkout instead, use
`zed-fork.url = "git+file:///home/drew/personal/zed?ref=code-review&shallow=1";`. That builds
the committed branch tip. Without `ref`, it builds the working tree, including dirty files.
`shallow=1` keeps Zed's full history out of Nix's git cache. **Never use
`path:/home/drew/personal/zed`**: it would copy the 40+ GB `target/` into the store.

#### Why no `follows`

Don't add `inputs.nixpkgs.follows = "nixpkgs"`. The fork pins its own nixpkgs, crane and
rust-overlay, so its derivation only changes when the fork changes:

- Your weekly `nix flake update` won't rebuild Zed (tens of minutes).
- Every host evaluates the same derivation, so one host's Cachix push serves all of them.

The cost is a second nixpkgs revision in Zed's runtime closure. The same reasoning rules out the
fork's `overlays.default`.

### 2. `overlays.nix`: make `pkgs.zed-editor` the fork (Linux only)

Add this inside the attribute set the overlay returns (it already receives `inputs`, and
`flake.nix:115` applies it), next to the similar `tdx` entry:

```nix
  # Drew's Zed fork (drew-council/zed, branch code-review) as a stable-channel app, so it takes
  # over nixpkgs Zed's database, CLI socket and desktop entry.
  zed-editor =
    let
      fork = inputs.zed-fork;
      channel = super.lib.trim (builtins.readFile "${fork}/crates/zed/RELEASE_CHANNEL");
    in
    if super.stdenv.hostPlatform.isLinux then
      assert super.lib.assertMsg (channel == "stable")
        "zed-fork: code-review is on a ${channel} tag; rebase it onto a stable vX.Y.Z tag first";
      fork.packages.${super.stdenv.hostPlatform.system}.default.override { releaseChannel = "stable"; }
    else
      super.zed-editor;
```

The assert fails at evaluation time, loudly, so a fork that sits on a preview tag never gets
installed as your stable Zed. Every reference to `pkgs.zed-editor` now gets the fork, including
`environment.systemPackages` in `modules/packages/graphical.nix:90`. Home-manager's
`programs.zed-editor` default gets it too, should you use it with the system `pkgs`. The macOS
host keeps nixpkgs' Zed (the fork's `releaseChannel` is Linux-only for anything but nightly).

This snippet was checked by evaluation against the local checkout (2026-10-17): on
`v1.25.0-pre` it stops with "code-review is on a preview tag"; with the channel forced to stable
it evaluates to `zed-editor-1.25.0` with `mainProgram = "zed"`.

### 3. `modules/packages/graphical.nix`: remove the `zed` alias

Delete the `zed = pkgs.symlinkJoin { … }` binding with its comment (lines 14–24) and the
`zed # alias` entry (line 91), and keep `zed-editor`. The fork already ships `bin/zed`, so the
alias's `ln -s "$out/bin/zeditor" "$out/bin/zed"` would fail with "File exists".

Nothing else needs to change:

- `home/xdg-mime.nix:15` (`editor = "dev.zed.Zed.desktop"`) matches the stable desktop entry.
- `vars.defaults.editor = "zeditor --new"` (`flake.nix:172`), the `zed = "zeditor"` shell alias
  (`home/shell.nix:29`) and lazygit's `editPreset = "zed"` (`home/programs/lazygit.nix:29`) keep
  working: the fork ships both binaries.

### Other configs (generic)

Without an overlay, for example in home-manager:

```nix
{ inputs, pkgs, ... }:
let
  zedFork =
    inputs.zed-fork.packages.${pkgs.stdenv.hostPlatform.system}.default.override {
      releaseChannel = "stable";
    };
in
{
  home.packages = [ zedFork ];
  # or: programs.zed-editor = { enable = true; package = zedFork; };
}
```

Whatever the config, remove nixpkgs' `zed-editor` from it. A second stable-channel Zed would
share the same database, settings and `zed-stable.sock`. Whichever app starts first owns the
socket, so the other's CLI opens files in it, and both claim `dev.zed.Zed.desktop`.

## Build and switch

```sh
cd ~/nixconf
nix flake lock                 # first time: adds zed-fork to flake.lock
nh os build                    # optional dry run
nh os switch
```

- **No `-j` caps.** The product build uses your normal Nix settings (`cores = 0`,
  `max-jobs = 16`). The agents' `-j8`/`nice` caps only apply to their cargo sessions. The machine
  will be busy, so switch when no agent is building.
- **Build time:** the first build compiles everything with release LTO. It hasn't been measured:
  the clean `release-fast` build took 20 minutes at `-j4` (DESIGN §8.1), and `release` is slower
  per core, but this build uses all cores.
- **Later builds:**
  - Any fork commit that touches the Nix source (`crates/`, `assets/`, `script/`, … — including a
    DESIGN.md or INSTALL.md edit) rebuilds the workspace crates.
  - Each upstream rebase also rebuilds the deps-only derivation, because the manifests change.

### First launch

1. Quit the running Zed (today nixpkgs' 1.23.2).
2. With Zed closed, back up its state:
   `cp -a ~/.local/share/zed/db ~/.local/share/zed/db.bak-$(date +%F)`. The fork migrates the
   database forward.
3. Start Zed from the launcher ("Zed") or with `zed .`, then check:
   - `zed --version` prints `Zed 1.X.Y+<sha7> <sha> – /nix/store/…/libexec/zed-editor`, with no
     `nightly` or `preview` word.
   - While Zed runs, `~/.local/share/zed/zed-stable.sock` exists.
   - The command palette offers `code_review: start review`.

### Dev builds after the switch

Once the fork is your stable Zed, a dev build from the checkout on a stable tag
(`target/release-fast/zed`) is also stable-channel: launched directly, it opens your real
database and hands its paths to your running Zed through `zed-stable.sock`. Run dev builds
through `script/live-harness/zh` or with `--user-data-dir <tmpdir>`.

## Updating after each rebase

Once the fork's `code-review` has been rebased and pushed:

```sh
cd ~/nixconf && nix flake update zed-fork && nh os switch
```

## Cachix (`bizmythy-nixconf.cachix.org`)

- **Already wired up.** The cache is a substituter on every host (`substituters_config.json`,
  read by `modules/base.nix`). Your `nh` wrapper (`pkgs/nh-cachix.nix`) pushes everything built
  during `nh os …` through `cachix watch-exec --watch-mode post-build-hook`. The first host that
  builds the fork uploads it, and the other NixOS hosts download it, as long as they evaluate the
  same derivation (hence no `follows`).
- **Watch the cache size.** The push includes crane's deps-only `cargoArtifacts`: a compressed
  `target/` of every third-party crate, rebuilt after every upstream rebase. Check the cache's
  storage limit and garbage collection.
- **`zed.cachix.org` won't help.** It's in your substituters and in the fork's `nixConfig`, but
  upstream CI doesn't push release Nix builds there, and the fork's derivations are unique anyway.
- **Later option:** a CI job on `drew-council/zed` could build and push after each rebase, so no
  host builds locally. It isn't set up.

## Trying it without installing

```sh
nix run github:drew-council/zed/code-review
```

This runs the unmodified `packages.default`, which is the **nightly** channel. It gets its own
database (`0-nightly`) and socket (`zed-nightly.sock`), and shares your settings and keymap, so it
runs safely next to the installed Zed. It builds just as long, uncapped. Use
`nix run /home/drew/personal/zed` to build the local checkout.

## Rollback

1. Roll back right away with `sudo nixos-rebuild switch --rollback`, or by booting the previous
   generation.
2. Revert the nixconf edits (steps 1–3) so the next switch doesn't reinstall the fork.
3. Data:
   - Zed's migrator only checks the migration steps a binary knows, so an older Zed should open
     the migrated `0-stable` database and ignore the fork's `code_review_*` tables.
   - If workspace restore misbehaves, restore `db.bak-<date>`.
   - Decline settings-migration prompts while you might still roll back.
