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
## `shell(command = "bash scripts/...")` wrappers — delegation
## defeats the engine's incremental-build, action-cache, per-test
## invalidation, and the CI sharding the engine grows into per
## ``reprobuild-specs/CI-Sharding.md``.
##
## On Windows the recipe drives real reprobuild tool provisioning via
## the tarball entries the ``uses:`` packages declare (cargo, rustc,
## rustfmt, nim, nimble, capnp). On Linux/macOS the Nix flake
## continues to supply the same toolchain. Either path produces
## byte-equivalent build outputs and the same test pass/fail set —
## CI cross-checks this through the side-by-side `ci.yml` (nix) +
## `ci-reprobuild.yml` (reprobuild) flow per Repo-Requirements §2.9.
##
## Cairo: cairo-lang-compiler corelib lookup uses `CAIRO_CORELIB_DIR`.
## The corelib is platform-independent Cairo source (a tree of `.cairo`
## files), not a binary, and the cairo-lang-* crates load it at runtime
## via the env var. M6 (reprobuild Windows migration) wires the fetch
## + extract step into the recipe via a typed `sh.shell` action below;
## the recipe then feeds the resolved corelib path into the cargo test
## edges via the typed-tool `extraEnv = @[("CAIRO_CORELIB_DIR", ...)]`
## parameter (MR10 closed the per-edge env-injection gap).

import os
import repro_project_dsl
import repro_dsl_stdlib/packages/sh

## M6 corelib provisioning (Windows migration of
## `ensure-cairo-corelib.ps1`):
## reprobuild's package DSL (repro_dsl_stdlib/packages/packages_schema)
## only models tool-binary provisioning — every `tarball` / `nixPackage`
## / `scoopApp` form requires an `executablePath`. A data-only package
## shape (a tarball whose payload is a tree of source files consumed by
## an env var at runtime, with no executable) doesn't fit. Until the
## schema gains a `dataPackage` / `headers_only` shape we land a
## recipe-local `sh.shell` action that downloads + extracts the corelib
## at build time. MR10 added per-edge env-var injection to the typed-
## tool DSL (`recordToolInvocation` / generated `cargo.test` wrappers
## now accept `extraEnv = openArray[(string, string)]`), so the
## `CAIRO_CORELIB_DIR` env var is now threaded directly into the cargo
## test edges below rather than relying on parent-process inheritance.
const CairoCorelibVersion = "2.17.0-rc.4"
  ## Mirrors `CAIRO_CORELIB_VERSION` in `windows/toolchain-versions.env`
  ## and the recorder's pin in `Cargo.toml`'s `cairo-lang-*` crates.

const CairoCorelibSrcDir =
  "build/cairo-corelib/" & CairoCorelibVersion & "/corelib/src"
const CairoCorelibMarker = CairoCorelibSrcDir & "/lib.cairo"
  ## `lib.cairo` is the canonical entry point cairo-lang-semantic's
  ## helper.rs reads first; its presence is the "extraction succeeded"
  ## signal the recipe's other edges depend on.

package codetracer_cairo_recorder:
  uses:
    # Rust toolchain — declared by version so the tarball-direct
    # provisioning entries in repro_dsl_stdlib/packages/cargo.nim /
    # rustc.nim / rustfmt.nim resolve on Windows. On Linux/macOS the
    # nix flake supplies the same versions.
    "rustc >=1.85"
    "cargo >=1.85"

    # Nim toolchain — codetracer_trace_writer_nim's build.rs compiles
    # a static library at cargo build time.
    "nim >=2.2 <3.0"
    "nimble"

    # Cap'n Proto schema compiler used by the recorder's build.rs.
    "capnp"

    # libzstd headers + library, needed when linking the Nim FFI
    # static library into the cargo build.
    "zstd"

    # `sh` (POSIX shell) drives the M6 cairo-corelib extraction edge
    # below. Available on Windows via Git for Windows (`sh.exe` ships
    # in PortableGit at `bin/sh.exe`); resolved by the standard `sh`
    # package's tarball/scoop slice.
    "sh"

    # pkg-config + OpenSSL — openssl-sys consults pkg-config to find
    # OpenSSL on Linux/macOS. The Windows build uses the rustls-tls
    # feature instead so neither is on the windows toolchain floor.
    when not defined(windows):
      # Cargo build scripts look for ``cc`` by default; pass ``CC=clang``
      # below and make clang part of the Unix dev environment.
      "clang"
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
    const binarySuffix = (when defined(windows): ".exe" else: "")
    const recorderBinary =
      "target/release/codetracer-cairo-recorder" & binarySuffix
    let cargoCompilerEnv: seq[(string, string)] =
      when defined(windows): @[]
      else: @[("CC", "clang")]

    let recorderBuild = cargo.build(
      locked = true,
      release = true,
      actionId = "codetracer-cairo-recorder.cargo-build",
      extraInputs = @[
        "Cargo.toml", "Cargo.lock",
        "src", "build.rs"
      ],
      extraOutputs = @[recorderBinary],
      extraEnv = cargoCompilerEnv)
    discard collect("default", @[recorderBuild])

    # ---- Cairo corelib fetch + extract edge ---------------------------
    #
    # M6 (reprobuild Windows migration of `ensure-cairo-corelib.ps1`):
    # the cairo-lang-compiler crates need the Cairo standard library
    # source tree at runtime (resolved via `CAIRO_CORELIB_DIR`). Until
    # the package schema grows a data-only shape (see "M6 gap" note at
    # the top of this file), the recipe drives the fetch + extract via
    # a typed `sh.shell` action. Output: `build/cairo-corelib/<ver>/
    # corelib/src/lib.cairo` (the marker the downstream tests' input
    # set tracks). The script is bash-portable and uses curl + tar,
    # both present on Windows 10+ (via the OS-bundled `curl.exe` and
    # `tar.exe`) and on Linux/macOS hosts.
    #
    # Source: starkware-libs/cairo releases tarball
    # `release-x86_64-unknown-linux-musl.tar.gz` (the corelib is
    # platform-independent Cairo source; the Linux musl asset is the
    # smallest reliable mirror of the tree).
    let corelibExtract = shell(
      command =
        "set -euo pipefail; " &
        "ver=" & CairoCorelibVersion & "; " &
        "out=" & CairoCorelibSrcDir & "; " &
        "if [ -f \"$out/lib.cairo\" ]; then exit 0; fi; " &
        "mkdir -p build/cairo-corelib; " &
        "tmp=$(mktemp -d); " &
        "url=\"https://github.com/starkware-libs/cairo/releases/download/v${ver}/release-x86_64-unknown-linux-musl.tar.gz\"; " &
        "curl -fsSL -o \"$tmp/cairo.tar.gz\" \"$url\"; " &
        "stage=\"build/cairo-corelib/${ver}\"; " &
        "rm -rf \"$stage\"; " &
        "mkdir -p \"$stage\"; " &
        "tar -xzf \"$tmp/cairo.tar.gz\" -C \"$stage\" --strip-components=1 cairo/corelib; " &
        "rm -rf \"$tmp\"; " &
        "test -f \"$out/lib.cairo\"",
      actionId = "codetracer-cairo-recorder.cairo-corelib-extract",
      extraOutputs = @[CairoCorelibMarker])

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
    # The cairo-corelib marker is declared as a test-edge input so the
    # engine sequences cairo-corelib extraction before cargo test (the
    # integration tests panic without `lib.cairo` reachable from
    # `CAIRO_CORELIB_DIR`).

    # MR10: thread CAIRO_CORELIB_DIR into the cargo test edges directly
    # via the typed-tool `extraEnv` parameter. The corelib path is
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
        "src", "build.rs", "tests",
        CairoCorelibMarker
      ],
      extraOutputs = @["target/debug/deps"],
      extraEnv = cairoTestEnv)

    let testsRun = cargo.test(
      locked = true,
      actionId = "codetracer-cairo-recorder.cargo-test-run",
      after = @[testsBuild.action, corelibExtract],
      extraInputs = @[
        "Cargo.toml", "Cargo.lock",
        "src", "tests",
        "target/debug/deps",
        CairoCorelibMarker
      ],
      extraEnv = cairoTestEnv)

    discard collect("test", @[testsRun.action])
