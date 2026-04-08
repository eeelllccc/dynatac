#!/usr/bin/env bash
set -e

EXAMPLE="$1"

cargo build --example "$EXAMPLE"
espflash flash "target/xtensa-esp32s3-espidf/debug/examples/$EXAMPLE" --monitor
