# lantern host daemon

Build the phone helper first so `src/daemon/session.rs` can embed `assets/helper.jar`:

```sh
source ~/.local/opt/pd-toolchain/env.sh
python3 tools/build.py helper
cd host && cargo build --release -p lantern
```

A missing jar is a compile-time error. `tools/build.py helper` copies the release jar to
`host/lantern/assets/helper.jar`. At session start the daemon installs it only when the SHA-256 does
not match, at the persistent content-addressed path
`/data/local/tmp/lantern-helper-<full-sha256>.jar`; no other Lantern jar is left behind.
