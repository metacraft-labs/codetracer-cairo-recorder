## Reprobuild dev env + build recipe for codetracer-cairo-recorder.
##
## Mirrors the dev shell declared in ``flake.nix`` (Linux/macOS) and
## the Windows DIY env declared in ``env.ps1``. ``repro build`` /
## ``repro test`` reproduce the same artefacts and the same test set
## that ``just build`` / ``just test`` produce today.
##
## Per ``codetracer-specs/Repo-Requirements.md`` §2.8 the recipe
## expresses build and test execution NATIVELY through typed-tool
## edges (`cargo.build`, `cargo.test`). It does NOT delegate to
## `shell(command = "bash scripts/...")` wrappers for the Rust build /
## test — delegation defeats the engine's incremental-build,
## action-cache, per-test invalidation, and the CI sharding the engine
## grows into per ``reprobuild-specs/CI-Sharding.md``. Two ``sh.shell``
## edges below wrap the NON-cargo steps ``just test`` also runs (the
## cairo-corelib provisioning and the CLI-convention verification
## script), so ``repro test`` reproduces the FULL ``just test`` set
## rather than a subset.
##
## On Windows the recipe drives real reprobuild tool provisioning via
## the tarball entries the ``uses:`` packages declare (cargo, rustc,
## rustfmt, nim, nimble, capnp). On Linux/macOS the Nix flake
## continues to supply the same toolchain. Either path produces
## byte-equivalent build outputs and the same test pass/fail set —
## CI cross-checks this through the side-by-side `ci.yml` (nix) +
## `ci-reprobuild.yml` (reprobuild) flow per Repo-Requirements §2.9.
##
## **This repo is a Rust CONSUMER of two sibling crates, but NOT a
## reprobuild ``uses: "<sibling>"`` consumer.** The recorder's
## ``Cargo.toml`` pulls in two crates from the sibling
## ``codetracer-trace-format`` repo via cargo ``path`` dependencies:
## ``codetracer_trace_types`` and ``codetracer_trace_writer_nim``. Both
## are resolved and compiled INSIDE cargo — out of reprobuild's reach —
## so they are NOT reprobuild library-threaded ``uses:`` consumptions
## (the SC-11 develop-mode src-threading applies only to reprobuild's
## own ``nim.c`` edges, and ``codetracer-trace-format`` is a Rust
## workspace, not a Nim-library sibling in the AVAILABLE set). This
## matches how the sibling recorders (circom, evm, fuel, …) model the
## identical dependency: the toolchain floor for the Nim FFI that
## ``codetracer_trace_writer_nim``'s ``build.rs`` compiles at cargo
## build time (``nim`` + ``nimble`` + ``capnp`` + ``zstd``) is declared
## in ``uses:``, and cargo does the cross-crate wiring itself. The
## recorder crate itself has NO ``build.rs`` — the only cargo-build
## inputs are the manifest, the lock, and the ``src`` tree.
##
## **Per-test platform gating.** ``just test`` is ``cargo test
## --locked`` followed by the CLI-convention shell script — no test
## FILE in this repo carries a per-host gate. All five integration test
## files (``test_cli.rs``, ``test_column_aware.rs``, ``test_ctfs_audit.rs``,
## ``test_replay_rpc.rs``, ``test_tracer.rs``) plus the ``src`` unit
## tests contain NO ``#[cfg(target_os = …)]`` / ``#[ignore]`` selection:
## they compile ``.cairo`` fixtures via the bundled cairo-lang-* crates
## and decode the produced ``.ct`` container through the
## ``codetracer-trace-format-nim`` ``ct-print`` binary on EVERY host.
## ``test_replay_rpc.rs`` exercises a localhost mock JSON-RPC server
## (``TcpListener::bind("127.0.0.1:0")`` — no external network egress),
## so it too runs unconditionally. The single whole-workspace
## ``cargo.test`` execute edge therefore matches the repo's own ``just
## test`` one-for-one — there is no per-OS partition to model. The shell
## verify edge is POSIX-portable (``bash``) and likewise unconditional.
##
## **Tool provisioning.** ``defaultToolProvisioning "path"`` matches the
## canonical Rust-recorder recipes: the nix dev shell puts ``cargo`` /
## ``rustc`` / ``nim`` / ``nimble`` / ``capnp`` / ``zstd`` on ``PATH``
## (and ``PKG_CONFIG_PATH`` for libzstd + openssl), so the weak-local
## PATH resolver is the right default. Without it ``repro build``
## refuses to run with "typed tool provisioning is required for uses
## declarations".
##
## **Cairo corelib.** The cairo-lang-compiler crates need the Cairo
## standard library source tree at runtime, resolved via the
## ``CAIRO_CORELIB_DIR`` env var (the recorder's integration tests
## compile ``.cairo`` fixtures and panic without ``lib.cairo``
## reachable). The nix dev shell + ``ci.yml`` provision it by fetching
## the corelib source pinned to the ``cairo-lang-*`` crate version. The
## recipe reproduces that step as a typed ``sh.shell`` edge (the package
## DSL only models tool-binary provisioning, so a data-only source tree
## consumed by an env var has no ``tarball``/``nixPackage`` shape yet)
## and threads the resolved absolute path into the cargo test edges via
## the typed-tool ``extraEnv`` parameter (MR10 per-edge env injection).

import os
import repro_project_dsl
import repro_dsl_stdlib/packages/sh

## Cairo corelib provisioning.
## Mirrors ``CAIRO_CORELIB_VERSION`` in ``windows/toolchain-versions.env``
## and the recorder's pin in ``Cargo.toml``'s ``cairo-lang-*`` crates.
const CairoCorelibVersion = "2.17.0-rc.4"

const CairoCorelibSrcDir =
  "build/cairo-corelib/" & CairoCorelibVersion & "/corelib/src"
const CairoCorelibMarker = CairoCorelibSrcDir & "/lib.cairo"
  ## ``lib.cairo`` is the canonical entry point cairo-lang-semantic's
  ## corelib loader reads first; its presence is the "extraction
  ## succeeded" signal the recipe's test edges depend on.

package codetracer_cairo_recorder:
  defaultToolProvisioning "path"

  uses:
    # Rust toolchain — declared by version so the tarball-direct
    # provisioning entries in repro_dsl_stdlib/packages/cargo.nim /
    # rustc.nim resolve on Windows. On Linux/macOS the nix flake
    # supplies the same versions.
    "rustc >=1.85"
    "cargo >=1.85"
    "just >=1"

    # Nim toolchain — the sibling ``codetracer_trace_writer_nim`` crate's
    # build.rs compiles a Nim FFI static library at cargo build time via
    # ``nim c``; ``nimble`` resolves that FFI's nimble requirements.
    "nim >=2.2 <3.0"
    "nimble"

    # Cap'n Proto schema compiler used by the trace-format crates'
    # build.rs (``capnpc`` over the trace schema).
    "capnp"

    # libzstd headers + library, needed when linking the Nim FFI
    # static library into the cargo build (the FFI's C output
    # ``#include``s ``zstd.h`` and the CBOR+Zstd writer links libzstd).
    "zstd"

    # POSIX shell — drives the cairo-corelib provisioning edge and the
    # CLI-convention verification edge below, the same
    # ``bash tests/verify-cli-convention-no-silent-skip.sh`` step
    # ``just test`` runs after ``cargo test``.
    "sh"

    # pkg-config + OpenSSL — openssl-sys consults pkg-config to find
    # OpenSSL on Linux/macOS. The Windows build uses the rustls-tls
    # feature instead so neither is on the windows toolchain floor.
    when defined(linux):
      # Nim staticlib builds invoked from cargo expect a GNU archiver on
      # Linux. Use gcc so Nim selects ``ar`` instead of ``llvm-ar``.
      "gcc"
    when defined(macosx):
      # Cargo build scripts look for ``cc`` by default; pass ``CC=clang``
      # below and make clang part of the macOS dev environment.
      "clang"
    when not defined(windows):
      "pkg-config"
      "openssl"

  executable codetracerCairoRecorder:
    name: "codetracer-cairo-recorder"

  devEnv:
    activity "default"

  build:
    # ---- Primary build edge (the `default` collection) ----------------
    #
    # Native cargo build for the recorder binary. Enrolled into the
    # conventional ``default`` collection per
    # reprobuild-specs/Build-Graph-Collections.md §"`default`"; this
    # makes ``repro build`` (no positional target) materialise this
    # edge's closure.
    #
    # ``locked = true`` because this repo DOES check in ``Cargo.lock``:
    # the build must fail rather than silently regenerate the lock if a
    # member's ``Cargo.toml`` or a sibling path-dep's resolution drifts
    # from the pinned lock.
    #
    # The recorder has no ``build.rs`` of its own; the only inputs are
    # the manifest, the lock, and the ``src`` tree. The sibling
    # trace-format crates cargo pulls in via ``path`` deps are tracked
    # per-crate at action-end by cargo's own ``.d`` depfiles under
    # ``target/*/deps`` (the makeDepfile dependency policy the cargo
    # package declares).
    const binarySuffix = (when defined(windows): ".exe" else: "")
    const recorderBinary =
      "target/release/codetracer-cairo-recorder" & binarySuffix
    let cargoCompilerEnv: seq[(string, string)] =
      when defined(windows): @[]
      elif defined(macosx): @[("CC", "clang")]
      else: @[("CC", "gcc")]

    let recorderBuild = cargo.build(
      locked = true,
      release = true,
      actionId = "codetracer-cairo-recorder.cargo-build",
      extraInputs = @[
        "Cargo.toml", "Cargo.lock",
        "src"
      ],
      extraOutputs = @[recorderBinary],
      extraEnv = cargoCompilerEnv)
    discard collect("default", @[recorderBuild])

    # ---- Cairo corelib fetch + extract edge ---------------------------
    #
    # The cairo-lang-compiler crates need the Cairo standard library
    # source tree at runtime (resolved via ``CAIRO_CORELIB_DIR``).
    # Reproduces the ``ci.yml`` "Fetch Cairo corelib" step as a typed
    # ``sh.shell`` action. Output:
    # ``build/cairo-corelib/<ver>/corelib/src/lib.cairo`` (the marker the
    # downstream test edges track). The script is bash-portable and uses
    # ``curl`` + ``tar``, both present on Windows 10+ (via the
    # OS-bundled ``curl.exe`` and ``tar.exe``) and on Linux/macOS hosts.
    #
    # Source: the ``starkware-libs/cairo`` source tag tarball
    # (``archive/refs/tags/v<ver>.tar.gz``), whose ``corelib/`` tree is
    # platform-independent Cairo source — the same asset ``ci.yml``
    # pulls. Idempotent: re-extraction is skipped when ``lib.cairo``
    # already exists.
    let corelibExtract = shell(
      command =
        "set -euo pipefail; " &
        "ver=" & CairoCorelibVersion & "; " &
        "out=" & CairoCorelibSrcDir & "; " &
        "if [ -f \"$out/lib.cairo\" ]; then exit 0; fi; " &
        "mkdir -p build/cairo-corelib; " &
        "tmp=$(mktemp -d); " &
        "url=\"https://github.com/starkware-libs/cairo/archive/refs/tags/v${ver}.tar.gz\"; " &
        "curl -fsSL -o \"$tmp/cairo.tar.gz\" \"$url\"; " &
        "stage=\"build/cairo-corelib/${ver}\"; " &
        "rm -rf \"$stage\"; " &
        "mkdir -p \"$stage\"; " &
        "tar -xzf \"$tmp/cairo.tar.gz\" -C \"$stage\" --strip-components=1 \"cairo-${ver}/corelib\"; " &
        "rm -rf \"$tmp\"; " &
        "test -f \"$out/lib.cairo\"",
      actionId = "codetracer-cairo-recorder.cairo-corelib-extract",
      extraOutputs = @[CairoCorelibMarker])

    let ctPrintBuild = shell(
      command =
        "set -euo pipefail; " &
        "recorder_root=\"$PWD\"; " &
        "zstd_flags=\"\"; " &
        "zstd_lib_dir=\"\"; " &
        "if command -v nix >/dev/null 2>&1; then " &
          "zstd_dev=\"$(nix build --no-link --print-out-paths nixpkgs#zstd.dev 2>/dev/null || true)\"; " &
          "zstd_lib=\"$(nix build --no-link --print-out-paths nixpkgs#zstd.lib 2>/dev/null || true)\"; " &
          "zstd_out=\"$(nix build --no-link --print-out-paths nixpkgs#zstd 2>/dev/null || true)\"; " &
          "if [ -n \"$zstd_dev\" ] && [ -f \"$zstd_dev/include/zstd.h\" ]; then " &
            "zstd_flags=\"$zstd_flags --passC:-I$zstd_dev/include\"; " &
          "fi; " &
          "if [ -n \"$zstd_lib\" ] && [ -d \"$zstd_lib/lib\" ]; then " &
            "zstd_lib_dir=\"$zstd_lib/lib\"; " &
            "zstd_flags=\"$zstd_flags --passL:-L$zstd_lib/lib\"; " &
          "elif [ -n \"$zstd_out\" ] && [ -d \"$zstd_out/lib\" ]; then " &
            "zstd_lib_dir=\"$zstd_out/lib\"; " &
            "zstd_flags=\"$zstd_flags --passL:-L$zstd_out/lib\"; " &
          "fi; " &
        "fi; " &
        "case \"$zstd_flags\" in *--passL:-L*) ;; *) " &
          "IFS=:; for zstd_lib in ${LD_LIBRARY_PATH:-}:${DYLD_LIBRARY_PATH:-}; do " &
            "if [ -n \"$zstd_lib\" ] && { [ -f \"$zstd_lib/libzstd.dylib\" ] || " &
                "[ -f \"$zstd_lib/libzstd.so\" ] || [ -f \"$zstd_lib/libzstd.a\" ]; }; then " &
              "zstd_lib_dir=\"$zstd_lib\"; " &
              "zstd_flags=\"$zstd_flags --passL:-L$zstd_lib\"; break; " &
            "fi; " &
          "done; unset IFS; " &
        "esac; " &
        "if [ -z \"$zstd_flags\" ] && command -v pkg-config >/dev/null 2>&1; then " &
          "for flag in $(pkg-config --cflags libzstd 2>/dev/null || true); do " &
            "zstd_flags=\"$zstd_flags --passC:$flag\"; " &
          "done; " &
          "for flag in $(pkg-config --libs libzstd 2>/dev/null || true); do " &
            "zstd_flags=\"$zstd_flags --passL:$flag\"; " &
          "done; " &
        "fi; " &
        "if [ -z \"$zstd_flags\" ]; then " &
          "IFS=:; for zstd_lib in ${LD_LIBRARY_PATH:-}:${DYLD_LIBRARY_PATH:-}; do " &
            "zstd_include=\"${zstd_lib%/lib}/include\"; " &
            "if [ -n \"$zstd_lib\" ] && [ -f \"$zstd_include/zstd.h\" ]; then " &
              "zstd_lib_dir=\"$zstd_lib\"; " &
              "zstd_flags=\"--passC:-I$zstd_include --passL:-L$zstd_lib\"; break; " &
            "fi; " &
          "done; unset IFS; " &
        "fi; " &
        "if [ -z \"$zstd_flags\" ]; then " &
          "zstd_bin=\"$(command -v zstd 2>/dev/null || command -v zstd.exe 2>/dev/null || true)\"; " &
          "zstd_root=\"${zstd_bin%/*}\"; " &
          "if [ -n \"$zstd_bin\" ] && [ -f \"$zstd_root/include/zstd.h\" ]; then " &
            "zstd_flags=\"--passC:-I$zstd_root/include --passL:-L$zstd_root/dll " &
              "--passL:-L$zstd_root/static\"; " &
            "if [ -d \"$zstd_root/dll\" ]; then zstd_lib_dir=\"$zstd_root/dll\"; " &
            "elif [ -d \"$zstd_root/static\" ]; then zstd_lib_dir=\"$zstd_root/static\"; fi; " &
          "fi; " &
        "fi; " &
        "if [ -n \"$zstd_lib_dir\" ]; then " &
          "export LIBRARY_PATH=\"$zstd_lib_dir${LIBRARY_PATH:+:$LIBRARY_PATH}\"; " &
          "export NIX_LDFLAGS=\"-L$zstd_lib_dir ${NIX_LDFLAGS:-}\"; " &
        "fi; " &
        "echo \"ct-print zstd flags: ${zstd_flags:-<none>}\"; " &
        "cd ../codetracer-trace-format-nim; " &
        "if [ -d \"$recorder_root/.reprobuild-src/libs/results/src\" ] && " &
            "[ -d \"$recorder_root/.reprobuild-src/libs/nim-stew/src\" ]; then " &
          "nim c -d:release --mm:arc -p:src $zstd_flags " &
            "-p:\"$recorder_root/.reprobuild-src/libs/results/src\" " &
            "-p:\"$recorder_root/.reprobuild-src/libs/nim-stew/src\" " &
            "-o:ct-print src/codetracer_ct_print.nim; " &
        "else " &
          "nimble install -y stew results; " &
          "nim c -d:release --mm:arc -p:src $zstd_flags -o:ct-print " &
            "src/codetracer_ct_print.nim; " &
        "fi; " &
        "test -f ct-print" & binarySuffix,
      actionId = "codetracer-cairo-recorder.ct-print-build",
      cacheable = false)

    # ---- Test-binary build + run edges (the `test` collection) -------
    #
    # Two-stage shape per Repo-Requirements.md §2.8: `cargo.test(noRun =
    # true)` builds every cargo test binary into
    # `target/debug/deps/<crate>-<hash>` (the engine tracks the deps
    # directory as the build edge's effect set because the hashed
    # filename floats with input content); `cargo.test(noRun = false)`
    # then runs the binaries in one cargo invocation. The execute edge
    # depends on the build edge so the engine only re-runs tests when
    # an input changed since the last successful execution.
    #
    # Per-test execute edges fall out automatically once the
    # ct-test-runner cargo adapter lands per
    # reprobuild-specs/Test-Edges-And-Parallel-Runner.milestones.org
    # §M4 — the whole-binary edge becomes a fan-out point without
    # changing this recipe.
    #
    # ``test-programs/`` is a declared input because every integration
    # test opens a ``.cairo`` / ``.json`` fixture under it via
    # ``CARGO_MANIFEST_DIR`` (e.g. ``test-programs/cairo/flow_test.cairo``,
    # ``test-programs/starknet/mock_tx_trace.json``). The cairo-corelib
    # marker is declared as a test-edge input so the engine sequences
    # corelib extraction before the cargo test build/run (the integration
    # tests panic without ``lib.cairo`` reachable from
    # ``CAIRO_CORELIB_DIR``).
    #
    # MR10: thread CAIRO_CORELIB_DIR into the cargo test edges directly
    # via the typed-tool ``extraEnv`` parameter. The corelib path is
    # resolved to an absolute form at recipe-evaluation time because
    # cargo's cwd (the recorder crate root) differs from the recipe's
    # project root at runtime; the cairo-lang-* crates open the path
    # without re-rooting it against any base directory.
    let cairoCorelibAbsDir = absolutePath(CairoCorelibSrcDir)
    let cairoCorelibEnv = @[("CAIRO_CORELIB_DIR", cairoCorelibAbsDir)]
    let cairoTestEnv = cargoCompilerEnv & cairoCorelibEnv

    let testsBuild = cargo.test(
      locked = true,
      noRun = true,
      actionId = "codetracer-cairo-recorder.cargo-test-build",
      after = @[corelibExtract],
      extraInputs = @[
        "Cargo.toml", "Cargo.lock",
        "src", "tests", "test-programs",
        CairoCorelibMarker
      ],
      extraOutputs = @["target/debug/deps"],
      extraEnv = cairoTestEnv)

    let testsRun = cargo.test(
      locked = true,
      actionId = "codetracer-cairo-recorder.cargo-test-run",
      after = @[testsBuild.action, corelibExtract, ctPrintBuild],
      extraInputs = @[
        "Cargo.toml", "Cargo.lock",
        "src", "tests", "test-programs",
        "target/debug/deps",
        CairoCorelibMarker
      ],
      extraEnv = cairoTestEnv)

    # ---- CLI-convention verification edge -----------------------------
    #
    # ``just test`` runs ``bash
    # tests/verify-cli-convention-no-silent-skip.sh`` after ``cargo
    # test``. The script asserts the recorder's ``--help`` / ``--version``
    # surface complies with ``Recorder-CLI-Conventions.md`` (no
    # ``--format`` leak, ``--out-dir`` / ``ct print`` present, the two
    # env-var fallbacks referenced in source). It is not a cargo target,
    # so it is modelled as its own ``sh.shell`` execute edge rather than
    # dropped — reproducing the repo's full ``just test`` set. The script
    # itself does ``cargo build --locked --quiet`` (a no-op once the
    # recorder is built), then runs the freshly-built debug binary at
    # ``target/debug/codetracer-cairo-recorder``; ``after`` the cargo
    # test-build edge guarantees that binary exists before the script
    # runs. Non-cacheable: the script inspects a runtime binary via
    # automatic monitoring and asserts on ``--help`` text, so it is
    # re-run every ``repro test`` pass (matching ``just test``).
    let cliVerifyCommand =
      when defined(windows):
        "bash tests/verify-cli-convention-no-silent-skip.sh"
      elif defined(macosx):
        "CC=clang bash tests/verify-cli-convention-no-silent-skip.sh"
      else:
        "CC=gcc bash tests/verify-cli-convention-no-silent-skip.sh"
    let cliVerify = shell(
      command = cliVerifyCommand,
      actionId = "codetracer-cairo-recorder.verify-cli-convention",
      after = @[testsBuild.action],
      extraInputs = @[
        "tests/verify-cli-convention-no-silent-skip.sh",
        "Cargo.toml", "Cargo.lock", "src"
      ],
      cacheable = false)

    discard collect("test", @[testsRun.action, cliVerify])
