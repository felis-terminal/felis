---
title: Install
sidebar:
  order: 1
---

Install felis, its `xterm-felis` terminfo entry, and its shell completions, from the Nix flake, through home-manager,
through Homebrew, or from source.

**Prerequisites:** the [Nix package manager](https://nixos.org/download) with flakes enabled for the flake,
home-manager, and source-build paths; the source build takes its toolchain from the same flake. The Homebrew path needs
[Homebrew](https://brew.sh). The Linux and macOS archives and the Windows artifact need no Nix, and a native Windows
build needs only a Rust toolchain. Nothing else: felis needs no background service to configure, since the daemon starts
on first use.

## Install from the flake

```sh
# Try it without installing anything.
nix run github:felis-terminal/felis

# Or install it to your profile, then just run `felis`.
nix profile install github:felis-terminal/felis
felis
```

The package bundles all three binaries (`felis`, `felis-client`, `felis-daemon`) in one output, along with the
`xterm-felis` terminfo entry in `share/terminfo`, shell completions, and `felis(1)` man pages. On Linux the GUI client
is wrapped with required runtime GPU and windowing libraries, and a desktop entry in `share/applications` lists felis in
application launchers.

For offline documentation, the flake's `docs` package installs the markdown documentation tree under `share/doc/felis`,
readable with a markdown pager like `glow`:

```sh
nix profile install 'github:felis-terminal/felis#docs'
glow ~/.nix-profile/share/doc/felis/
```

## home-manager

The flake exposes a home-manager module that installs the package, writes `config.toml` from Nix, and points
`TERMINFO_DIRS` at the compiled terminfo so ncurses apps inside felis recognize `TERM=xterm-felis`. `installTerminfo`
(default `true`) governs that last entry; set it `false` if you manage your terminfo tree yourself.

```nix
{
  inputs.felis.url = "github:felis-terminal/felis";

  # In your home-manager configuration:
  imports = [ inputs.felis.homeManagerModules.felis ];

  programs.felis = {
    enable = true;
    # Optional: written where felis reads it on each platform:
    # $XDG_CONFIG_HOME/felis/config.toml on Linux,
    # ~/Library/Application Support/felis/config.toml on macOS
    # (see the configuration page for the schema).
    settings = {
      font.family = "JetBrainsMono Nerd Font";
      font.size_px = 14.0;
      theme.foreground = "#e5e5e5";
      theme.background = "#0d0d12";
    };
  };
}
```

The module can also run a notification subscriber as a user service; see
[Get notified when a job finishes](enable-notifications.md).

[Stylix](https://github.com/danth/stylix) users can additionally import `inputs.felis.homeManagerModules.stylix` to
drive felis's palette and font from the active base16 or base24 scheme and the window background opacity from
`stylix.opacity.terminal`. Stylix has no backdrop knob, so set `programs.felis.settings.window.backdrop` yourself
alongside a translucent opacity (`"blur"` on macOS, one of the DWM materials on Windows); see `window.backdrop` in
[the configuration reference](../reference/config.md) for supported values.

## Homebrew

On Apple silicon with macOS 15 or later, and on x86_64 Linux, the felis tap installs a prebuilt bottle:

```sh
brew install felis-terminal/tap/felis
```

The formula puts `felis` on your `PATH` and installs the `xterm-felis` entry under `share/terminfo`, shell completions
for bash, zsh and fish, and the `felis(1)` man pages. Where no bottle applies, as on an earlier macOS, Homebrew builds
the formula from source. Of the platforms Homebrew runs on, felis supports only these two
([workspace.md](../reference/workspace.md) "Build and platform matrix").

`felis` adds the formula's terminfo directory to `TERMINFO_DIRS` for the sessions it starts. A window opened from Finder
does not pass through it, so for those point ncurses at the entry from your shell's startup file:

```sh
export TERMINFO_DIRS="$(brew --prefix felis)/share/terminfo:"
```

On macOS the formula also builds `felis.app` inside its prefix. Link it into `/Applications` to open felis from Finder
or the Dock:

```sh
ln -sf "$(brew --prefix felis)/felis.app" /Applications/felis.app
```

## Build from source

To build from source with `cargo`, use the repository Nix flake to pin the toolchain (nightly Rust plus runtime graphics
libraries):

```sh
git clone https://github.com/felis-terminal/felis
cd felis
nix develop          # dev shell; installs the pre-commit hook

cargo run -p felis-daemon -- serve &
cargo run -p felis-client          # the GUI directly, or:
cargo run -p felis-cli             # the `felis` front-door (execs felis-client)
```

On Linux, add the `wayland-clipboard` feature (`cargo build --features wayland-clipboard`; the Nix flake's Linux package
sets it). It wires `arboard` through `wl-clipboard-rs`, so Wayland sessions on wlroots-based compositors (Sway,
Hyprland, niri) and KDE Plasma reach the system clipboard through `wlr_data_control_v1`, which is privileged and
focus-independent: a copy issued while the window is unfocused still lands. A build without the feature falls back to
XWayland's X11 selection, which is also what GNOME sessions use, since Mutter does not expose `wlr_data_control_v1`.
With XWayland disabled that path fails, and felis reports `ClipboardError::Unavailable` and falls back to an in-process
clipboard. The feature is off by default because macOS and Windows builds have no use for `wl-clipboard-rs` or its
transitive dependencies.

Installing outside Nix requires setting up the terminfo entry, shell completions, and Linux runtime library wrappers
manually. Compile the terminfo entry once with `tic`:

```sh
tic -x share/terminfo/felis.terminfo   # writes into ~/.terminfo
```

Verify the installation with `infocmp -x xterm-felis`. For terminfo overrides and environment configuration, see
[terminal identity](../reference/terminal-identity.md).

Platform build targets and CI validation gates are specified in
[workspace.md](../reference/workspace.md#build-and-platform-matrix).

## The Linux archive

`x86_64-linux` binaries ship as a release asset that runs on a host without Nix:

1. Open the [release page](https://github.com/felis-terminal/felis/releases) for the version you want.
2. Download `felis-x86_64-linux.tar.gz` and unpack it wherever you can write:

   ```sh
   mkdir -p ~/.local/opt && tar -xzf felis-x86_64-linux.tar.gz -C ~/.local/opt
   ~/.local/opt/felis-x86_64-linux/bin/felis --version
   ```

3. Run `bin/felis`, or put that directory on your `PATH`, or link the launchers you use into a directory already on it:

   ```sh
   ln -s ~/.local/opt/felis-x86_64-linux/bin/felis ~/.local/bin/felis
   ```

Move the tree whole and keep it together: `bin/` holds the three launchers, the loader they run, and nothing the
binaries can find once they are apart. The archive needs no Nix installation; it carries its own C library, X11 and
Wayland libraries, and runs them through its own loader wherever you unpack it.

Three things come from the host:

- **A Vulkan driver and `libvulkan.so.1`.** The client renders through it and reports no adapter without one, which
  `felis doctor` names. On Debian and Ubuntu that is `libvulkan1` plus the driver package for your GPU
  (`mesa-vulkan-drivers` for AMD and Intel).
- **A monospace font.** felis reads the host's fontconfig configuration for the `monospace` alias and the font
  directories, and falls back to any installed fixed-pitch face when the alias names nothing installed. On Debian and
  Ubuntu, `fonts-dejavu-core` is enough; `font.family` picks another face ([configuration](../reference/config.md)).
- **A C library no newer than the bundled one.** The host's driver libraries are loaded into a process already running
  the archive's glibc 2.42, which works as long as they were linked against 2.42 or older. Every current distribution
  release is; a host newer than the archive is the case to watch, and the fix is a newer archive.

The compiled `xterm-felis` entry is at `share/terminfo`, and the launchers in `bin/` add that directory to
`TERMINFO_DIRS`, so programs in the sessions felis starts recognize `TERM=xterm-felis` with no setup. Only a program
that reads the entry outside those sessions, such as a `tmux` server started elsewhere, needs a copy in your own tree:

```sh
cp -r ~/.local/opt/felis-x86_64-linux/share/terminfo/. ~/.terminfo/
```

Shell completions are under `share/bash-completion`, `share/zsh` and `share/fish`, and the `felis(1)` man pages under
`share/man`; install them the way your shell and `MANPATH` expect.

The desktop entry that lists felis in application launchers is `share/applications/felis.desktop`. Its `Exec=felis` runs
whatever `felis` your `PATH` finds, so put `bin/felis` there first (step 3), then copy the entry where launchers look:

```sh
mkdir -p ~/.local/share/applications
cp ~/.local/opt/felis-x86_64-linux/share/applications/felis.desktop ~/.local/share/applications/
```

Application launchers take their `PATH` from the graphical session, not from your shell's startup files. If only your
shell puts `bin/felis` on `PATH`, set `Exec=` in the copy to its absolute path instead.

## The macOS archive

`aarch64-darwin` binaries ship as a release asset that runs on a host without Nix:

1. Open the [release page](https://github.com/felis-terminal/felis/releases) for the version you want.
2. Download `felis-aarch64-darwin.tar.gz` and unpack it wherever you can write:

   ```sh
   mkdir -p ~/Applications/felis && tar -xzf felis-aarch64-darwin.tar.gz -C ~/Applications/felis --strip-components=1
   xattr -dr com.apple.quarantine ~/Applications/felis
   ~/Applications/felis/bin/felis --version
   ```

3. Run `bin/felis`, or put that directory on your `PATH`, or link `bin/felis` into a directory already on it. Open
   `felis.app` from Finder for a window.

The archive is ad-hoc signed, not notarized, so macOS quarantines everything it extracts from a download and refuses to
launch it. Clearing the attribute over the whole tree, as above, is what makes both the app and the CLI runnable.
Right-clicking `felis.app` and choosing **Open** works as well, but it authorizes only the app's main executable, not
the separately signed `felis` inside it, so `bin/felis` still needs the `xattr` command.

Move the tree whole and keep it together: `bin/felis` runs the CLI inside `felis.app`, where it sits beside the client
and the daemon it spawns. The app carries every library it loads from outside the system, so it needs no Nix
installation.

The compiled `xterm-felis` entry is at `share/terminfo`. `bin/felis` adds that directory to `TERMINFO_DIRS`, so the
sessions it starts need no setup; a window opened from Finder does not pass through it. For those, either point ncurses
at the entry from your shell's startup file, or copy it into your own tree once:

```sh
export TERMINFO_DIRS="$HOME/Applications/felis/share/terminfo:"
# or, once:
cp -r ~/Applications/felis/share/terminfo/. ~/.terminfo/
```

Shell completions are under `share/bash-completion`, `share/zsh` and `share/fish`, and the `felis(1)` man pages under
`share/man`; install them the way your shell and `MANPATH` expect.

## The Windows deliverable

Windows binaries ship as a release asset:

1. Open the [release page](https://github.com/felis-terminal/felis/releases) for the version you want.
2. Download `felis-x86_64-pc-windows-msvc.zip`, which contains `felis.exe`, `felis-client.exe`, and `felis-daemon.exe`.
3. Unpack all three executables into a single directory on your `PATH`. The front door locates `felis-client.exe` and
   `felis-daemon.exe` as siblings of itself.

For a build newer than the last release, the `windows` workflow uploads the same zip as a run artifact for each passing
commit on `main`. Alternatively, build from source on Windows with `cargo build --release`.

## Installing a specific release

To install a specific release version, specify the tag in the flake reference:

```sh
nix profile install 'github:felis-terminal/felis/v0.1.0'
```

Each tagged release attaches the Linux and macOS archives, the Windows zip, and `felis-config.schema.json` and
`felis.proto`, which pin the config keys and the wire schema that build accepts. For the release verification gate
specification, see [workspace.md](../reference/workspace.md#release-gate).
