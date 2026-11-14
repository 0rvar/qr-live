# One image that holds every toolchain of the quine relay (Ubuntu 23.10
# package set of quine-relay commit 26066d1) and runs qr-live in exec mode.
#
#   docker build --platform linux/amd64 -f docker/Dockerfile -t qr-live:dev .
#
# Layers are ordered from slowest and most stable (apt, upstream vendor
# tools, package caches) to cheapest (qr-live binaries, reference files), so
# a change to qr-live only rebuilds the last few layers.

ARG QUINE_RELAY_COMMIT=26066d1ede0b91720baceecbd99dbdf7e4d3240b

# ---------------------------------------------------------------------------
# qr-live binaries. Bookworm's glibc (2.36) is older than Ubuntu 23.10's
# (2.38), so the binaries run in the final stage.
FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY web ./web
COPY languages.yaml ./
# The target directory is a cache mount that outlives the copied sources, so
# the sources are touched to make cargo see them as newer than its last build.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    find src web languages.yaml Cargo.toml -exec touch {} + \
 && cargo build --release --locked --bins \
 && mkdir -p /out \
 && cp target/release/qr-live target/release/single target/release/all /out/

# ---------------------------------------------------------------------------
# Every language toolchain, as in the upstream Dockerfile at 26066d1.
FROM ubuntu:23.10 AS toolchain
ENV DEBIAN_FRONTEND=noninteractive

# Ubuntu 23.10 is end of life; its packages live on old-releases.
RUN sed -i -E 's#https?://(archive|security)\.ubuntu\.com/ubuntu/?#http://old-releases.ubuntu.com/ubuntu/#g' \
      /etc/apt/sources.list \
 && if [ -d /etc/apt/sources.list.d ]; then \
      find /etc/apt/sources.list.d -type f -exec sed -i -E 's#https?://(archive|security)\.ubuntu\.com/ubuntu/?#http://old-releases.ubuntu.com/ubuntu/#g' {} +; \
    fi \
 && rm -f /etc/dpkg/dpkg.cfg.d/excludes \
 && apt-get update && apt-get upgrade -y \
 && apt-get install -y --no-install-recommends apt-utils moreutils ca-certificates git tini \
 && apt-get clean && rm -rf /var/lib/apt/lists/*

# The package list of the upstream Dockerfile, in a few batches so a failed
# batch does not invalidate the others.
RUN apt-get update && apt-get install -y \
      afnix algol68g aplus-fsf aspectj asymptote ats2-lang bash bc \
      bison bsdgames bsh clisp clojure cmake coffeescript crystal \
      curl dafny dc dhall dotnet7 elixir emacs-nox erlang \
      f2c fish flex fp-compiler \
 && apt-get clean && rm -rf /var/lib/apt/lists/*
RUN apt-get update && apt-get install -y \
      g++ gambas3-gb-pcre gambas3-scripter gap gawk gcc gdb gdc \
      genius gforth gfortran ghc ghostscript gm2 gnat gnucobol4 \
      gnuplot gobjc golang gpt groff groovy guile-3.0 gzip \
      haxe icont iconx intercal iverilog jasmin-sable jq kotlin \
 && apt-get clean && rm -rf /var/lib/apt/lists/*
# Mono's package scripts (gacutil) run under mono's interpreter: its amd64
# JIT aborts when the process is mapped above the 2 GiB offset range, as it
# is under Rosetta. The interpreter gives the same result on real hardware.
RUN apt-get update && MONO_ENV_OPTIONS=--interpreter apt-get install -y \
      ksh libevent-dev libgd-dev libpng-dev libpolyml-dev lisaac livescript llvm \
      lua5.3 m4 make maxima minizinc mono-devel nasm neko \
      nickle nim node-typescript nodejs ocaml octave openjdk-11-jdk pari-gp \
 && apt-get clean && rm -rf /var/lib/apt/lists/*
RUN apt-get update && MONO_ENV_OPTIONS=--interpreter apt-get install -y \
      parser3-cgi perl php-cli polyml python3 r-base rakudo ratfor \
      rc regina-rexx ruby ruby-mustache rustc scala scilab-cli sed \
      slsh spin surgescript swi-prolog tcl tcsh valac vim \
      wabt xsltproc yabasic yorick zoem zsh \
 && apt-get clean && rm -rf /var/lib/apt/lists/*

# Upstream quine-relay at the pinned commit, with its vendored interpreters
# (lci, npiet, cfunge, compilechef, lazyk, spl2c, Vlt.exe) built into
# vendor/local.
ARG QUINE_RELAY_COMMIT
RUN git clone https://github.com/mame/quine-relay /usr/local/share/quine-relay \
 && cd /usr/local/share/quine-relay \
 && git checkout --detach "$QUINE_RELAY_COMMIT" \
 && test "$(git rev-parse HEAD)" = "$QUINE_RELAY_COMMIT" \
 && rm -rf .git \
 && make -C vendor

ENV PATH=/usr/local/share/quine-relay/vendor/local/bin:/usr/games:$PATH \
    DOTNET_NOLOGO=1 \
    DOTNET_CLI_TELEMETRY_OPTOUT=1 \
    DOTNET_SKIP_FIRST_TIME_EXPERIENCE=1 \
    DOTNET_CLI_WORKLOAD_UPDATE_NOTIFY_DISABLE=1 \
    DOTNET_GENERATE_ASPNET_CERTIFICATE=false \
    NUGET_PACKAGES=/opt/qr-cache/nuget \
    GOTOOLCHAIN=local

# The relay runs as an unprivileged user. HOME may be a tmpfs at run time,
# so caches that must survive that live under /opt/qr-cache.
RUN useradd --create-home --uid 10001 --shell /bin/bash qr \
 && mkdir -p /opt/qr-cache/nuget \
 && chown -R qr:qr /opt/qr-cache

# The three dotnet steps restore NuGet packages on first use. Restoring the
# same project shapes here fills NUGET_PACKAGES so the relay needs no
# network at run time. The runtime's W^X mapping is off for this restore
# only, because Rosetta cannot translate it; the relay itself keeps it.
USER qr
RUN export DOTNET_EnableWriteXorExecute=0; set -e; d=$(mktemp -d); cd "$d"; \
    for lang in vb cs fs; do \
      mkdir "$lang"; \
      case "$lang" in \
        vb) src=QR.vb; printf 'Module QR\nSub Main()\nSystem.Console.Write("ok")\nEnd Sub\nEnd Module\n' > "$lang/$src";; \
        cs) src=QR.cs; printf 'class QR { static void Main() { System.Console.Write("ok"); } }\n' > "$lang/$src";; \
        fs) src=QR.fsx; printf 'printf "ok"\n' > "$lang/$src";; \
      esac; \
      echo "<Project Sdk=\"Microsoft.NET.Sdk\"><PropertyGroup><OutputType>Exe</OutputType><TargetFramework>net7.0</TargetFramework><EnableDefaultCompileItems>false</EnableDefaultCompileItems></PropertyGroup><ItemGroup><Compile Include=\"$src\" /></ItemGroup></Project>" > "$lang/tmp.${lang}proj"; \
      (cd "$lang" && test "$(dotnet run --project "tmp.${lang}proj")" = ok); \
    done; \
    cd /; rm -rf "$d" "$HOME/.dotnet" "$HOME/.local" /tmp/NuGetScratch* 2>/dev/null || true
USER root

# ---------------------------------------------------------------------------
FROM toolchain AS runtime
COPY --from=build /out/qr-live /out/single /out/all /usr/local/bin/
COPY reference /opt/qr-live/reference
USER qr
WORKDIR /opt/qr-live
EXPOSE 3000
ENTRYPOINT ["/usr/bin/tini", "--", "qr-live", "--mode", "exec", "--toolchain", "upstream", \
            "--listen", "0.0.0.0:3000", "--reference-dir", "/opt/qr-live/reference", \
            "--min-step-ms", "4000"]
