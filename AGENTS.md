# qr-live

A live visualisation of mame's 128-language quine relay. A Rust server runs the relay lap after lap and pushes events to a browser page over SSE.

## Layout

- `languages.yaml` - one entry per language: input file, build and run commands, output file and its SHA-1. Embedded in the binary. The base values are written for the nix toolchain (`flake.nix`). An optional `upstream:` block holds only the fields that differ inside the upstream image (`work_input`, `build`, `build_output`, `build_file`, `env`, `command`); `build: ""` removes the build step, `env` entries are added to the base env, and `work_input` renames only the file written into the step's work directory (the ring and `reference/` keep using `input`).
- `docker/Dockerfile` - one image with every toolchain (Ubuntu 23.10 package set and vendored interpreters of upstream quine-relay commit 26066d1) that runs `qr-live --mode exec --toolchain upstream`. `docker/seccomp.json` is Docker's default seccomp profile plus the `personality` flags maxima needs.
- `reference/` - the 128 intermediate programs, matching upstream quine-relay commit 26066d1 (Ubuntu 23.10 version). Loaded into memory at startup; the server refuses to start if a file is missing or a reference output's SHA-1 differs from its `output_sha1`. After startup the directory is not read again.
- `src/relay.rs` - ring order, events, shared state, the runner (replay and exec).
- `src/server.rs` - axum router: static files, `/api/events` (SSE), `/api/source/{index}`.
- `web/` - the client (plain HTML, JS and CSS, no build step). Embedded in the binary with `include_str!`, so a change needs a rebuild.
- `src/bin/single.rs`, `src/bin/all.rs` - run and verify one step or every step against `reference/`.

## Run

```
cargo run --bin qr-live -- --listen 127.0.0.1:3000
```

- `--mode replay` (default) plays the reference files back with `--step-ms` per step. Needs no language toolchain.
- `--toolchain nix|upstream` (default `nix`, also on `single` and `all`) picks the command flavour; `upstream` applies the `upstream:` blocks.
- `--mode exec` runs each step for real. A step whose tool is not on PATH is replayed. A step that fails or produces the wrong hash is reported as failed and the relay continues from the reference output.
- `--docker-image IMAGE` runs each exec step inside that image (`docker run --rm --network none`, no resource limits by default).
- `--docker-arg ARG` (repeatable) adds an argument to `docker run` before the image name, e.g. `--docker-arg=--memory=4g --docker-arg=--user=1000:1000`.
- `--min-step-ms` is a floor: a step is shown for max(its real duration, the floor), so the host idles between quick steps.
- In exec mode each build and run phase has one deadline (`--step-timeout-secs`) covering the process and its output. Stdout over 64 MiB fails the step, as does an output pipe still held open 2 s after the shell exits.
- If the runner stops without a shutdown signal, the process exits non-zero so a supervisor can restart it.

## Docker image

Build (amd64; on another architecture add `--platform linux/amd64`, which runs everything emulated and slowly):

```
docker build -f docker/Dockerfile -t qr-live:latest .
```

The slow layers (apt, `make -C vendor`, the NuGet warm-up) come first and are cached, so a change to the Rust code or `languages.yaml` only rebuilds the binaries and the last layers. Upstream is cloned at the pinned commit during the build; nothing from upstream is kept in this repo.

The image runs as user `qr` (uid 10001) with tini as init. NuGet packages for the three dotnet steps are restored at build time into `/opt/qr-cache/nuget`, so the relay needs no network. `single` and `all` are in the image too (`--entrypoint all qr-live:latest --toolchain upstream`).

Run:

```
docker run -d --name qr-live --restart unless-stopped \
  -p 127.0.0.1:3000:3000 \
  --read-only \
  --tmpfs /tmp:rw,exec,nosuid,size=2g \
  --tmpfs /home/qr:rw,exec,nosuid,size=1g,uid=10001,gid=10001 \
  --memory 4g --memory-swap 4g --pids-limit 1024 --cpus 2 \
  --security-opt seccomp=$PWD/docker/seccomp.json \
  --security-opt no-new-privileges \
  qr-live:latest
```

- tmpfs contents count toward `--memory`. One lap under emulation reached the 4 GiB limit (`memory.peak`, page cache included) without a step failing for it; give it more if the host has room.
- The work directories live in `/tmp` and compilers write caches under `HOME`; both tmpfs mounts need `exec` because steps run the binaries they build.
- `docker/seccomp.json` is the only privilege beyond Docker's defaults. maxima (GCL) sets `READ_IMPLIES_EXEC` and `ADDR_NO_RANDOMIZE` on itself with `personality(2)` and re-executes, and the default profile rejects those values (`personality failure 1`). Upstream runs the whole image `--privileged` for this; the profile allows only those `personality` values. Without it every step but maxima still runs.
- The container needs no network after the build; it only has to publish the port.
- `--min-step-ms 4000` in the entrypoint holds every step at least 4 s; pass a different `--min-step-ms` after the image name to change it (arguments after the image replace the whole command line, so repeat the others too).

NixOS, behind a reverse proxy on the same host:

```nix
virtualisation.oci-containers.containers.qr-live = {
  image = "qr-live:latest";            # loaded with `docker load` or built on the host
  ports = [ "127.0.0.1:3000:3000" ];
  extraOptions = [
    "--read-only"
    "--tmpfs=/tmp:rw,exec,nosuid,size=2g"
    "--tmpfs=/home/qr:rw,exec,nosuid,size=1g,uid=10001,gid=10001"
    "--memory=4g" "--memory-swap=4g" "--pids-limit=1024" "--cpus=2"
    "--security-opt=seccomp=${./docker/seccomp.json}"
    "--security-opt=no-new-privileges"
  ];
};
```

Under emulation on Apple silicon (Rosetta) two runtimes need workarounds that real amd64 does not: `-e MONO_ENV_OPTIONS=--interpreter` (mono's JIT aborts on Rosetta's high addresses) and `-e DOTNET_EnableWriteXorExecute=0` (Rosetta cannot translate .NET's W^X mappings). maxima is OOM-killed there even with `--privileged`.

## Test

```
cargo test --test test_relay --test test_toolchains
```

`tests/test_language_spec.rs::test_tools_available` only passes inside the nix shell from `flake.nix`.
