#!/bin/bash

# -u ensures that referencing unset variables is an error
# -e ensures that the script dies if a command fails with a nonzero error code
set -ue

if !(hash "cargo") 2>/dev/null; then
  echo "siderophile requires cargo, which doesn't seem to be installed"
  exit 1
fi

## Cargo stuff
echo "building siderophile"
cargo build --release

# Where to look for `rustfilt`. If CARGO_HOME is set, use $CARGO_HOME/bin.
# Otherwise, use ~/.cargo/bin
CARGO_BIN=${CARGO_HOME:-~/.cargo}/bin

if !(PATH="$PATH:$CARGO_BIN" hash rustfilt) 2>/dev/null; then
    echo "didn't find rustfilt, installing it now"
    cargo install rustfilt
fi

echo "Done. Read README.md for further instructions"
