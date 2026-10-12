# Uses ThomasDickey fork as upstream freedesktop terminal-wg tree is dormant.
{
  stdenv,
  fetchFromGitHub,
  python3,
  runtimeShell,
}:

stdenv.mkDerivation {
  pname = "esctest2";
  version = "unstable-2025-08-24";

  src = fetchFromGitHub {
    owner = "ThomasDickey";
    repo = "esctest2";
    rev = "664be3cf2c1e3f06bc93a8bafb48a0db83c607db";
    hash = "sha256-JmUMvWmQoPyoWttW4K7Ap3/Tn0D3n8tHVPwprpeC+Is=";
  };

  nativeBuildInputs = [ python3 ];

  # The window-op cases sleep after each resize, iconify or move for a real
  # xterm's window manager to catch up. The harness applies them in stream
  # order, so the sleeps (26 s of the run) change no result.
  postPatch = ''
    substituteInPlace esctest/tests/xterm_winops.py \
      --replace-fail 'need_sleep = escargs.args.expected_terminal in ["xterm"]' 'need_sleep = False'
  '';

  dontBuild = true;

  installPhase = ''
    runHook preInstall
    mkdir -p $out/share/esctest $out/bin
    cp -r esctest/. $out/share/esctest/
    # Upstream ships python2.7 shebangs despite being Python 3 compatible.
    patchShebangs $out/share/esctest/esctest.py \
                  $out/share/esctest/esclog.py
    cat > $out/bin/esctest <<EOF
    #!${runtimeShell}
    exec ${python3}/bin/python3 \\
      $out/share/esctest/esctest.py "\$@"
    EOF
    chmod +x $out/bin/esctest
    runHook postInstall
  '';

  meta.description = "ESC sequence conformance suite (ThomasDickey fork)";
}
