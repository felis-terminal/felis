{
  lib,
  stdenv,
  craneLib,
  pkg-config,
  makeWrapper,
  installShellFiles,
  ncurses,
  desktop-file-utils,
  freetype,
  vulkan-loader,
  libxkbcommon,
  wayland,
  libGL,
  libx11,
  libxcursor,
  libxi,
  libxrandr,
  libxcb,
  # Default on Linux; dead weight on macOS where arboard already handles the system pasteboard.
  withWaylandClipboard ? stdenv.hostPlatform.isLinux,
  # Forwarded commit revision because build src excludes `.git`. Shape is
  # `<rev40>[-dirty]`, exactly what `git rev-parse HEAD` plus a dirty check
  # would have produced in the tree.
  gitHash ? null,
}:

let
  # dlopen'd at runtime (absent from DT_NEEDED), so plain rpath fails to find them.
  runtimeLibs = [
    freetype
    vulkan-loader
    libxkbcommon
    wayland
    libGL
    libx11
    libxcursor
    libxi
    libxrandr
    libxcb
  ];
  # gitHash omitted here to avoid invalidating cargoArtifacts on every commit.
  commonArgs = {
    pname = "felis";
    version = (lib.importTOML ../Cargo.toml).workspace.package.version;

    src = lib.fileset.toSource {
      root = ../.;
      fileset = lib.fileset.unions [
        ../Cargo.toml
        ../Cargo.lock
        ../crates
        ../share
        ../tests
      ];
    };

    strictDeps = true;

    nativeBuildInputs = [
      pkg-config
      makeWrapper
      installShellFiles
      ncurses
    ];

    buildInputs = [
      freetype
    ]
    ++ lib.optionals stdenv.hostPlatform.isLinux [
      vulkan-loader
      libxkbcommon
      wayland
      libGL
      libx11
      libxcursor
      libxi
      libxrandr
      libxcb
    ];

    cargoExtraArgs =
      "--locked" + lib.optionalString withWaylandClipboard " --features felis-client/wayland-clipboard";

    # Tests require PTY/renderer/display server absent from the build sandbox.
    doCheck = false;
  };

  cargoArtifacts = craneLib.buildDepsOnly commonArgs;
in
craneLib.buildPackage (
  commonArgs
  // {
    inherit cargoArtifacts;

    nativeBuildInputs =
      commonArgs.nativeBuildInputs ++ lib.optionals stdenv.hostPlatform.isLinux [ desktop-file-utils ];

    env = lib.optionalAttrs (gitHash != null) { FELIS_GIT_HASH = gitHash; };

    postInstall = ''
      bash ${./compile-terminfo.sh} share/terminfo/felis.terminfo "$out/share/terminfo"
    ''
    + lib.optionalString (stdenv.buildPlatform.canExecute stdenv.hostPlatform) ''
      installShellCompletion --cmd felis \
        --bash <("$out/bin/felis" completions bash) \
        --zsh  <("$out/bin/felis" completions zsh) \
        --fish <("$out/bin/felis" completions fish)

      "$out/bin/felis" __mangen man
      installManPage man/*.1
    ''
    + lib.optionalString stdenv.hostPlatform.isLinux ''
      desktop-file-validate share/applications/felis.desktop
      install -Dm644 share/applications/felis.desktop "$out/share/applications/felis.desktop"
    ''
    + lib.optionalString stdenv.hostPlatform.isDarwin ''
      # macOS requires a .app bundle to launch as a foreground app with Dock activation and HiDPI.
      bash ${./make-macos-app.sh} \
        --client "$out/bin/felis-client" \
        --daemon "$out/bin/felis-daemon" \
        --cli "$out/bin/felis" \
        --version "$version" \
        --out "$out/Applications"
    '';

    # Only the GUI client dlopens GPU / windowing libs; daemon and CLI are headless.
    postFixup = lib.optionalString stdenv.hostPlatform.isLinux ''
      wrapProgram "$out/bin/felis-client" \
        --prefix LD_LIBRARY_PATH : "${lib.makeLibraryPath runtimeLibs}"
    '';

    # Exposed so the MSRV check builds the exact source and inputs that ship,
    # rather than a second fileset free to drift away from this one.
    passthru = { inherit commonArgs; };

    meta = {
      description = "GPU-accelerated terminal emulator with a daemon/client split";
      homepage = "https://github.com/felis-terminal/felis";
      license = lib.licenses.asl20;
      mainProgram = "felis";
      # Explicit platforms over lib.platforms.unix to avoid advertising untested BSDs and Solaris.
      platforms = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
    };
  }
)
