# Lantern feasibility probes (throwaway)

These are not Lantern product code. The approved plan is in
`docs/superpowers/plans/2026-11-07-lantern-spikes.md`.

## Target and safety

Every ADB command targets **20e9318**. Actual preflight found **Android 17 / SDK 37,
LineageOS 24**, superseding the design's Android-16 assumption. No other device is selected
implicitly. Existing SDK 36 tools compile the probes; actual runtime checks are on SDK 37.

`spikes/baseline/pre-reset.json` records the phone before the user-approved reset of four old
freeform/desktop flags. The reset leaves these at zero deliberately. `baseline/before.json`
is the post-reset baseline against which temporary probe changes are compared. Neither file
is committed; both are mode 0600. A reboot was not performed, so boot-cached configuration
is not proven reset.

Probe commands can install a test APK, create/destroy owned displays, temporarily grant its
notification/secure-settings permissions, switch the IME and power off the panel. Do not run
two drivers simultaneously. Closing a driver is not a substitute for checking rollback.
Recovery journals live with each run under gitignored `spikes/raw/`. Full settings snapshots,
package inventories, recordings, notification identifiers and screenshots stay private.

The exported test command receiver requires the platform `DUMP` permission; ordinary apps
must not be able to send it IME commands. No probe sends a message to another person. Real
app text tests must remain unsubmitted and never press Send/Enter.

## Build and runs

```sh
python3 spikes/build.py             # signed APK + shell DEX jar; no installation
python3 spikes/snapshot.py save spikes/baseline/before.json  # refuses overwrite
python3 spikes/s1_encoders.py --matrix
python3 spikes/s6_resize.py --reps 20
python3 spikes/s4_focus.py --reps 20
python3 spikes/s5_ime.py
python3 spikes/s3_notif.py
python3 spikes/s7_compat.py
python3 spikes/s2_locked.py          # intentionally locks/sleeps phone; run last
```

The shell probe loads scrcpy's **unmodified 5.0.1 server DEX** as a second classpath entry to
reuse its runtime workarounds and hidden-API wrappers. This is a probe dependency, not a
choice to fork or embed the whole scrcpy server in Lantern. The installed server is at
`~/.local/opt/scrcpy-linux-x86_64-v5.0.1/scrcpy-server`. No upstream source is committed here.

## Interpretation

- Encoder FPS is decoded frame count / recorded duration, not end-to-end latency.
- A nonzero stream exit is not automatically encoder exhaustion: check the log. Stock
  scrcpy's default port range only fits five concurrent instances; explicit per-instance ports
  are required for the six-stream case.
- Resize time measures teardown/recreate to first encoded frame, not first *presented* frame
  on the PC. App relayout must also be checked against the app's timestamped logs.
- An IME heartbeat timeout proves only that recovery path; it is not a physical USB-yank test.
- A synthetic notification is not proof that every third-party app's PendingIntent targets
  the requested display.
- Gamepad device enumeration or injected key events are not proof of real controller input.
- Non-uniform screenshots are candidates for visual inspection, not compatibility passes.
- The system FFmpeg color-to-PNG path failed during setup. `-vf extractplanes=y` emits real
  grayscale frames for inspection without that conversion path.

Keep failed runs and observed errors; never replace them with plausible results. Findings
are written to `docs/spikes/RESULTS.md` with raw-run paths and explicit unverified checks.
