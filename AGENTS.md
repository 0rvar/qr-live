# qr-live

A live visualisation of mame's 128-language quine relay. A Rust server runs the relay lap after lap and pushes events to a browser page over SSE.

## Layout

- `languages.yaml` - one entry per language: input file, build and run commands, output file and its SHA-1. Embedded in the binary. The base values are written for the nix toolchain (`flake.nix`). An optional `upstream:` block holds only the fields that differ inside the upstream image (`work_input`, `build`, `build_output`, `build_file`, `env`, `command`); `build: ""` removes the build step, `env` entries are added to the base env, and `work_input` renames only the file written into the step's work directory (the ring and `reference/` keep using `input`).
- `docker/Dockerfile` - one image with every toolchain (Ubuntu 23.10 package set and vendored interpreters of upstream quine-relay commit 26066d1) that runs `qr-live --mode exec --toolchain upstream`. `docker/seccomp.json` is a copy of moby/profiles' default seccomp profile plus the `personality` flags maxima needs.
- `reference/` - the 128 intermediate programs, matching upstream quine-relay commit 26066d1 (Ubuntu 23.10 version). Loaded into memory at startup; the server refuses to start if a file is missing or a reference output's SHA-1 differs from its `output_sha1`. After startup the directory is not read again.
- `src/relay.rs` - ring order, events, shared state, the runner (replay and exec).
- `src/server.rs` - axum router: static files, `/api/events` (SSE), `/api/status`, `/api/source/{index}`.
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
- `--min-step-ms` is the minimum time from one step's start to the next. `step_done` is sent when the real work ends, with its real `duration_ms`; the runner then idles for the rest of the floor (the snapshot's `current` is null meanwhile) before the next `step_start`. A lap's `lap_done` follows its last `step_done` at once.
- `--max-failed-steps N` makes the process exit non-zero when a lap ends with more than N failed steps, so a supervisor restarts it (a fresh container starts with empty tmpfs and caches). Failing to create a step's work directory exits the same way at once. Unset, failures never stop the relay.
- `GET /api/status` is the endpoint for an uptime check: JSON `{run_id, mode, toolchain, started_at_ms, server_time_ms, lap, current, totals, recent_failures}`, where `recent_failures` holds the last 16 `step_done` payloads with `ok: false`, sent with `Cache-Control: no-store`. Alert on `totals.last_lap.failed_steps > 0`, or when `server_time_ms` is stale or the request fails.
- In exec mode each build and run phase has one deadline (`--step-timeout-secs`) covering the process and its output. Stdout over 64 MiB fails the step, as does an output pipe still held open 2 s after the shell exits.
- If the runner stops without a shutdown signal, the process exits non-zero so a supervisor can restart it.

## Docker image

Build (amd64; on another architecture add `--platform linux/amd64`, which runs everything emulated and slowly):

```
docker build -f docker/Dockerfile -t qr-live:latest .
```

The base images are pinned by digest (the tag is in a comment above each `FROM`). The slow layers (apt, `make -C vendor`, the NuGet warm-up) come first and are cached, so a change to the Rust code or `languages.yaml` only rebuilds the binaries and the last layer. Upstream is cloned at the pinned commit during the build; nothing from upstream is kept in this repo.

The image runs as user `qr` (uid 10001) with tini as init. NuGet packages for the three dotnet steps are restored at build time into `/opt/qr-cache/nuget`, so the steps need no network. `single` and `all` are in the image too (`--entrypoint all qr-live:latest --toolchain upstream`).

Base fields in `languages.yaml` apply to both toolchains, so after editing one run `all --toolchain upstream` in the image before deploying.

The entrypoint is `tini -- qr-live` and the default flags are the image's `CMD`: `--mode exec --toolchain upstream --listen 0.0.0.0:3000 --reference-dir /opt/qr-live/reference --min-step-ms 4000 --step-timeout-secs 180 --max-failed-steps 8`. Arguments after the image name replace that whole list, so repeat the flags you still want:

```
docker run ... qr-live:latest --mode exec --toolchain upstream --listen 0.0.0.0:3000 \
  --reference-dir /opt/qr-live/reference --min-step-ms 2000 --step-timeout-secs 180 --max-failed-steps 8
```

Run (from the repo root, since the seccomp path is relative to it):

```
docker run -d --name qr-live --restart unless-stopped \
  -p 127.0.0.1:3000:3000 \
  --read-only \
  --tmpfs /tmp:rw,exec,nosuid,size=2g \
  --tmpfs /home/qr:rw,exec,nosuid,size=1g,uid=10001,gid=10001 \
  --memory 4g --memory-swap 4g --pids-limit 1024 --cpus 2 \
  --cap-drop=ALL \
  --security-opt seccomp=$PWD/docker/seccomp.json \
  --security-opt no-new-privileges \
  --log-opt max-size=10m --log-opt max-file=3 \
  qr-live:latest
```

- tmpfs contents count toward `--memory`. One lap under emulation reached the 4 GiB limit (`memory.peak`, page cache included) without a step failing for it; give it more if the host has room.
- The work directories live in `/tmp` and compilers write caches under `HOME`; both tmpfs mounts need `exec` because steps run the binaries they build.
- `docker/seccomp.json` is the only privilege beyond Docker's defaults: moby/profiles `main` as of 2026-11-10 plus three `personality()` values. maxima (GCL) sets `READ_IMPLIES_EXEC` and `ADDR_NO_RANDOMIZE` on itself with `personality(2)` and re-executes, and the default profile rejects those values (`personality failure 1`). Upstream runs the whole image `--privileged` for this. The file is a frozen copy and does not follow future changes to Docker's default profile. Without it every step but maxima still runs.
- The relay steps run in the server's own network namespace. On the default bridge, every compiler and interpreter can reach the internet, although none needs to.
- On Docker before 28.0, a port published on `127.0.0.1` is also reachable from hosts on the same L2 segment ([moby#45610](https://github.com/moby/moby/issues/45610)).

**No egress** (alternative to `-p`): an internal network drops traffic to and from other networks, while the host can still reach container IPs directly. Point the reverse proxy at the container IP:

```
docker network create --internal --subnet 172.30.99.0/24 qr
docker run -d --name qr-live --network qr --ip 172.30.99.10 ...same flags without -p... qr-live:latest
# reverse proxy upstream: http://172.30.99.10:3000
```

Tested on OrbStack (Docker 29.8.2): `/api/status` answered 200 from the Docker host's network namespace and from macOS, and from inside the container both DNS (`old-releases.ubuntu.com`) and a direct connection to `1.1.1.1:80` failed. In NixOS, create the network in a oneshot unit (the module does not create networks) and add `--network=qr` and `--ip=172.30.99.10` to `extraOptions` instead of `ports`.

NixOS, behind a reverse proxy on the same host:

```nix
virtualisation.docker.enable = true;
virtualisation.oci-containers.backend = "docker";
virtualisation.oci-containers.containers.qr-live = {
  image = "qr-live:latest";            # loaded with `docker load` or built on the host
  ports = [ "127.0.0.1:3000:3000" ];
  extraOptions = [
    "--read-only"
    "--tmpfs=/tmp:rw,exec,nosuid,size=2g"
    "--tmpfs=/home/qr:rw,exec,nosuid,size=1g,uid=10001,gid=10001"
    "--memory=4g" "--memory-swap=4g" "--pids-limit=1024" "--cpus=2"
    "--cap-drop=ALL"
    "--security-opt=seccomp=${./docker/seccomp.json}"
    "--security-opt=no-new-privileges"
  ];
};
```

- The `backend` line is needed because `oci-containers` defaults to podman when `system.stateVersion` is 22.05 or later, and podman does not see images in Docker's store.
- `${./docker/seccomp.json}` resolves relative to the `.nix` file, so copy `docker/seccomp.json` next to the NixOS config (and track it in git when the config is a flake).
- The module logs to journald by default, so no log options are needed there.

Under emulation on Apple silicon (Rosetta) two runtimes need workarounds that real amd64 does not: `-e MONO_ENV_OPTIONS=--interpreter` (mono's JIT aborts on Rosetta's high addresses) and `-e DOTNET_EnableWriteXorExecute=0` (Rosetta cannot translate .NET's W^X mappings). Erlang/elixir segfault and maxima is OOM-killed there, even with `--privileged`.

Not yet verified on real amd64: elixir, erlang, maxima (with only the seccomp profile), mono's JIT steps and the three dotnet steps without the emulation variables, and the 4 GiB memory figure.

## Test

```
cargo test --test test_relay --test test_toolchains
```

`tests/test_language_spec.rs::test_tools_available` only passes inside the nix shell from `flake.nix`.
