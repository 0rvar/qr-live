#!/bin/bash

set -eo pipefail

LANGUAGE=$1
PKG=$2

# fallback pkg to LANGUAGE
if [ -z "$PKG" ]; then
  PKG=$LANGUAGE
fi

nix shell .#$PKG --command cargo run --bin single $LANGUAGE
