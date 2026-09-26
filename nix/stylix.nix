{
  config,
  lib,
  ...
}:

# `window.backdrop` is intentionally omitted: Stylix lacks a backdrop option, and deriving it
# from opacity < 1.0 would force an unrequested effect.
let
  cfg = config.stylix.targets.felis;
  colors = config.lib.stylix.colors.withHashtag;
in
{
  options.stylix.targets.felis.enable = config.lib.stylix.mkEnableTarget "felis" true;

  config = lib.mkIf (config.stylix.enable && cfg.enable) {
    programs.felis.settings = {
      font = {
        family = config.stylix.fonts.monospace.name;
        # Converts Stylix pt to logical px (4/3 ratio); passing pt directly rendered ~25% smaller than other terminals.
        size_px = config.stylix.fonts.sizes.terminal * 4.0 / 3.0;
      };

      window.opacity = config.stylix.opacity.terminal;

      cursor.color = colors.base05;

      theme = {
        foreground = colors.base05;
        background = colors.base00;

        palette = {
          black = colors.base00;
          red = colors.base08;
          green = colors.base0B;
          yellow = colors.base0A;
          blue = colors.base0D;
          magenta = colors.base0E;
          cyan = colors.base0C;
          white = colors.base05;
          bright_black = colors.base03;
          # bright-* mnemonics resolve to base24 bright slots when defined and fall back to base16 normal colors.
          bright_red = colors.bright-red;
          bright_green = colors.bright-green;
          bright_yellow = colors.bright-yellow;
          bright_blue = colors.bright-blue;
          bright_magenta = colors.bright-magenta;
          bright_cyan = colors.bright-cyan;
          bright_white = colors.base07;
        };
      };
    };
  };
}
