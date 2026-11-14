# qr-live

A live visualisation of mame's 128-language quine relay. A Rust server runs the relay lap after lap and pushes events to a browser page over SSE.

## Layout

- `languages.yaml` - one entry per language: input file, build and run commands, output file and its SHA-1. Embedded in the binary.
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
- `--mode exec` runs each step for real. A step whose tool is not on PATH is replayed. A step that fails or produces the wrong hash is reported as failed and the relay continues from the reference output.
- `--docker-image IMAGE` runs each exec step inside that image (`docker run --rm --network none`, no resource limits by default).
- `--docker-arg ARG` (repeatable) adds an argument to `docker run` before the image name, e.g. `--docker-arg=--memory=4g --docker-arg=--user=1000:1000`.
- In exec mode each build and run phase has one deadline (`--step-timeout-secs`) covering the process and its output. Stdout over 64 MiB fails the step, as does an output pipe still held open 2 s after the shell exits.
- If the runner stops without a shutdown signal, the process exits non-zero so a supervisor can restart it.

## Test

```
cargo test --test test_relay
```

`tests/test_language_spec.rs::test_tools_available` only passes inside the nix shell from `flake.nix`.
