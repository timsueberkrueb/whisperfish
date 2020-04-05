#!/bin/bash

set -e

echo Building for SailfishOS-$SFOS_VERSION-$TARGET

export MERSDK=/srv/mer
export MER_TARGET="SailfishOS-$SFOS_VERSION"
export RUSTFLAGS="-C link-args=-Wl,-rpath-link,$MERSDK/targets/$MER_TARGET-$TARGET/usr/lib/,-rpath-link,$MERSDK/targets/$MER_TARGET-$TARGET/lib/"

# Rust
curl --proto '=https' --tlsv1.2 -sSf -o ~nemo/rustup.sh https://sh.rustup.rs
sh ~nemo/rustup.sh \
    --profile minimal \
    -y \

# Host CC
sudo zypper install -y gcc gcc-c++

source $HOME/.cargo/env

source .ci/$TARGET.sh
rustup target add $RUSTUP_TARGET
export TARGET_CFLAGS="-isystem $MERSDK/targets/$MER_TARGET-$TARGET/usr/include/"

cat <<EOF > ~/.cargo/config
[build]
target = "$RUSTUP_TARGET"

[target.$RUSTUP_TARGET]
linker = "$RUSTUP_CC"
ar = "$RUSTUP_AR"
EOF

cargo build

cargo rpm build

sudo cp -r target/$RUSTUP_TARGET/release/rpmbuild/RPMs RPMS
