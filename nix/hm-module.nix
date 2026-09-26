{
  config,
  lib,
  pkgs,
  craneLib,
  ...
}:

let
  cfg = config.programs.felis;
  ncfg = cfg.notifications;
  tomlFormat = pkgs.formats.toml { };

  # terminal-notifier lacks an urgency knob, so macOS drops urgency rather than faking one.
  linuxNotifier = pkgs.writeShellApplication {
    name = "felis-notifier";
    runtimeInputs = [ pkgs.libnotify ];
    text = ''
      notify-send --urgency="''${3:-normal}" -- "$1" "$2"
    '';
  };

  macosNotifier = pkgs.writeShellApplication {
    name = "felis-notifier";
    runtimeInputs = [ pkgs.terminal-notifier ];
    text = ''
      terminal-notifier -title "$1" -message "$2"
    '';
  };

  # Spliced before the verb because clap rejects global `--host` after trailing subcommands.
  hostArg = lib.optionalString (ncfg.host != null) " --host ${lib.escapeShellArg ncfg.host}";

  # Runs out-of-process over public IPC so felis binaries avoid linking notification daemons.
  # `--format jsonl` is required because default human output is unparseable prose.
  notifyBridge = pkgs.writeShellApplication {
    name = "felis-notify";
    runtimeInputs = [
      cfg.package
      pkgs.jq
    ];
    text = ''
      felis${hostArg} notifications subscribe --format jsonl | while read -r evt; do
        # Notifications omit `event` key; checking `has("event")` avoids enumerating present and future event types.
        if [ "$(printf '%s' "$evt" | jq -r 'has("event")')" != false ]; then
          continue
        fi
        ${lib.optionalString ncfg.onlyDetached ''
          if [ "$(printf '%s' "$evt" | jq -r .attached)" = true ]; then
            continue
          fi
        ''}
        title=$(printf '%s' "$evt" | jq -r '.title // .session_title // "felis"')
        body=$(printf '%s' "$evt" | jq -r '.body // ""')
        urgency=$(printf '%s' "$evt" | jq -r '.urgency // "normal"')
        ${lib.getExe ncfg.notifier} "$title" "$body" "$urgency" || true
      done
    '';
  };
in
{
  options.programs.felis = {
    enable = lib.mkEnableOption "felis, a GPU-accelerated terminal emulator";

    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.callPackage ./package.nix { inherit craneLib; };
      defaultText = lib.literalExpression "felis package built from this flake";
      description = ''
        The felis package to install. It bundles both the `felis` client
        and the `felis-daemon` (the client auto-spawns the daemon off
        `$PATH`, so both must come from the same package) and ships the
        compiled `xterm-felis` terminfo entry.
      '';
    };

    installTerminfo = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = ''
        Add the package's compiled terminfo to `TERMINFO_DIRS` in the
        session environment, so programs outside felis sessions (a tmux
        server started elsewhere, `felis doctor`) recognize
        `TERM=xterm-felis`; the daemon hands the entry to its own sessions
        regardless. The trailing empty entry preserves the compiled-in
        default search path (system + `~/.terminfo`).
      '';
    };

    settings = lib.mkOption {
      type = tomlFormat.type;
      default = { };
      example = lib.literalExpression ''
        {
          font = {
            family = "JetBrainsMono Nerd Font";
            size_px = 14.0;
            features = [ "calt" "liga" ];
          };
          theme = {
            foreground = "#e5e5e5";
            background = "#0d0d12";
          };
          clipboard.use_os_clipboard = true;
        }
      '';
      description = ''
        Configuration written verbatim to the path the felis loader
        reads — {file}`$XDG_CONFIG_HOME/felis/config.toml` on Linux,
        {file}`~/Library/Application Support/felis/config.toml` on
        macOS. See {file}`docs/reference/config.md` for the full schema
        (`[font]`, `[theme]`, `[theme.palette]`, `[clipboard]`,
        `[window]`, `[keymap]`, `[cursor]`, `[mouse]`, `[shader]`).

        A launch started with `felis --config PATH` reads that file
        instead, so a one-off profile needs no change here.
      '';
    };

    notifications = {
      enable = lib.mkEnableOption "the felis desktop-notification relay" // {
        description = ''
          Run a user service (a systemd user unit on Linux, a launchd
          agent on macOS) that streams `felis notifications subscribe`
          and relays each decoded OSC 9 / 99 / 777 desktop notification
          to the OS notifier.

          Off by default and independent of `programs.felis.enable`:
          felis itself never spawns a notification helper — doing so
          would make it a notification daemon, the role
          {file}`docs/explanation/protocols/notifications.md` keeps one process
          boundary away. Enabling this option is the *user* declaring
          that consumer; the felis binaries still link no notification
          backend.
        '';
      };

      onlyDetached = lib.mkOption {
        type = lib.types.bool;
        default = true;
        description = ''
          Pop up only notifications from *detached* sessions
          (`attached == false`). An attached session already flashes its
          own window (the BEL-parity attention felis raises itself), so
          relaying it too would double the alert. Set false to surface
          every session's notifications.
        '';
      };

      host = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        example = "user@remote";
        description = ''
          Subscribe to a *remote* daemon over SSH
          (`felis --host <host> notifications subscribe`) instead of the
          local one. The value is any SSH destination `ssh` accepts — a
          `user@host` or a `~/.ssh/config` alias. Cross-host attach
          bridges every connection to the remote's persistent per-UID
          daemon, so the relay sees the same sessions a `--host` window
          attaches through, surfacing a remote program's notification on
          your local desktop ({file}`docs/reference/protocols/notifications.md`
          "Cross-host"). `null` (the default) keeps the relay on the
          local daemon.
        '';
      };

      notifier = lib.mkOption {
        type = lib.types.package;
        default = if pkgs.stdenv.hostPlatform.isDarwin then macosNotifier else linuxNotifier;
        defaultText = lib.literalExpression ''
          a `notify-send` wrapper on Linux, a `terminal-notifier` wrapper on macOS
        '';
        description = ''
          Program the bridge runs once per notification, invoked as
          `<notifier> <title> <body> <urgency>` where urgency is one of
          `low` / `normal` / `critical`. Override with any executable
          honoring that three-argument contract — e.g. a
          `writeShellApplication` wrapping `osascript`. The default pulls
          in `libnotify` (Linux) or `terminal-notifier` (macOS).
        '';
      };
    };
  };

  config = lib.mkMerge [
    (lib.mkIf cfg.enable {
      home.packages = [ cfg.package ];

      # `directories::ProjectDirs` on macOS ignores XDG variables and reads ~/Library/Application Support/felis.
      # A symlink from XDG was rejected to avoid teaching users an unsupported path.
      xdg.configFile."felis/config.toml" =
        lib.mkIf (cfg.settings != { } && !pkgs.stdenv.hostPlatform.isDarwin)
          {
            source = tomlFormat.generate "felis-config.toml" cfg.settings;
          };

      home.file."Library/Application Support/felis/config.toml" =
        lib.mkIf (cfg.settings != { } && pkgs.stdenv.hostPlatform.isDarwin)
          {
            source = tomlFormat.generate "felis-config.toml" cfg.settings;
          };

      home.sessionVariables = lib.mkIf cfg.installTerminfo {
        TERMINFO_DIRS = "${cfg.package}/share/terminfo:";
      };
    })

    (lib.mkIf (ncfg.enable && pkgs.stdenv.hostPlatform.isDarwin) {
      launchd.agents.felis-notify = {
        enable = true;
        config = {
          ProgramArguments = [ (lib.getExe notifyBridge) ];
          RunAtLoad = true;
          KeepAlive = true;
        };
      };
    })

    (lib.mkIf (ncfg.enable && !pkgs.stdenv.hostPlatform.isDarwin) {
      systemd.user.services.felis-notify = {
        Unit = {
          Description = "Relay felis desktop notifications to the OS";
          # Needs graphical session bus to reach notify-send.
          After = [ "graphical-session.target" ];
          PartOf = [ "graphical-session.target" ];
        };
        Service = {
          ExecStart = lib.getExe notifyBridge;
          # `always` over `on-failure`: daemon shutdown ends cleanly (exit 0) and relay must reconnect on next daemon run.
          Restart = "always";
          RestartSec = 5;
        };
        Install.WantedBy = [ "graphical-session.target" ];
      };
    })
  ];
}
