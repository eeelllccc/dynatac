#!/usr/bin/env bash
set -e

EXAMPLE="$1"

cd device
cargo build --example "$EXAMPLE"
cd ..
espflash flash "target/xtensa-esp32s3-espidf/debug/examples/$EXAMPLE" --monitor
