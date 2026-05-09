# dynatac

Text-based OS for the LilyGo T-Deck-Pro (ESP32-S3).

## Flashing

```sh
./flash.sh               # build and flash the OS
./flash-example.sh <name>  # build and flash a device example
```

## Testing

```sh
cargo test -p dynatac-core   # host tests (no hardware needed)
```
