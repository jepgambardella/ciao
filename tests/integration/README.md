# Integration checks

The integration directory is intentionally dependency-free. From the
workspace root, build the CLI once and run the smoke script:

```sh
cargo build -p ciao
tests/integration/smoke-detection.sh
```
