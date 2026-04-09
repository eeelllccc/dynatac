#!/usr/bin/env bash
set -e

cd device
cargo build
cd ..
espflash flash "target/xtensa-esp32s3-espidf/debug/dynatac" --monitor
