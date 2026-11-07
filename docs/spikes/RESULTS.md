# Lantern risk spikes — results

Date: 2026-11-07 · Device: Xiaomi 11T Pro `20e9318` · **Android 17 (SDK 37), LineageOS 24**
(the phone was updated since the design's Android 16 assumption) · scrcpy 5.0.1 · no root.
Before testing, the four old freeform/desktop developer flags were set to 0 with Sahaj's approval and
left at 0. No reboot was done.

Raw evidence (recordings, logs, private settings snapshots) is in gitignored `spikes/raw/` and
`spikes/baseline/`. The run IDs are given below.

## Summary

| Risk (spec §9) | Result | Key number |
|---|---|---|
| 1 Encoder concurrency | **Pass** | 6 simultaneous 1280×800 HW streams: 55–57 fps each. Up to 5: ~59.7 fps. 3 at 1080p: 59.8 fps |
| 6 Resize cost | **Pass (borderline)** | Median 144–156 ms from resize to first encoded frame (80/80, no errors). App relayout median 108 ms |
| 2 Locked phone | **Pass with ALWAYS_UNLOCKED** (awake + swipe keyguard). **Asleep: fails** | Normal flags: 1 frame, keyguard clock rendered. ALWAYS_UNLOCKED: 710 frames of live app |
| 4 Focus routing | **Pass** | `setFocusedTask` and tap-to-focus both routed 40/40 unattributed key events to the target display. `setFocusedDisplay` was rejected |
| 5 Hidden IME | **Pass (probe app)** | Exact `Xin chào, Tiếng Việt ạ 👋` plus composing text under IME policy LOCAL, FALLBACK and HIDE. Self-restore of the user's keyboard 5 s after heartbeat loss |
| 3 Notification click → display | **Pass (own notification)** | The PendingIntent sent with `setLaunchDisplayId` resumed on the Lantern display |
| 7 App compatibility | **No refusals seen** | 5/5 launchable apps rendered (2 games at splash/updating screens) |

### Design impact

- **Codec:** both work. H.265 had no losses up to 5 streams and degraded evenly at 6 (55 fps). H.264
  also held at 5 and gave 56.6 fps at 6. Keep "prefer HEVC" from spec §12. The degrade ladder in
  spec §9 is unlikely to trigger for light multitasking. A **cap of 6 windows** is a safe default.
- **Display flags:** use scrcpy 5's set, **including `ALWAYS_UNLOCKED`** (the shell may set it on this
  ROM) and `OWN_FOCUS`. Without `ALWAYS_UNLOCKED`, a Lantern display shows the lock screen.
- **The phone must stay awake:** a dozing/asleep phone produced no frames even with
  `ALWAYS_UNLOCKED`. This confirms spec §8.2: keep the phone awake and power the panel off (scrcpy
  `--turn-screen-off` style). Never send the sleep key.
- **Resize:** about 150 ms per resize with full encoder recreate, so debouncing during drags
  (spec §8.3) is required. Encoder reuse via reconfigure was **not tried**; it is a possible
  improvement.
- **Focus:** use `IActivityTaskManager.setFocusedTask(taskId)` when the PC window gains focus. It's
  cheaper than an injected tap and has no visible side effects.
- **IME:** the hidden companion IME plus display policy HIDE (2) works, and the app-side watchdog
  restore works. Keep the design.
- **Notifications:** firing the stored PendingIntent with `launchDisplayId` works for a PendingIntent
  the probe created itself. Third-party PendingIntents are **unverified**; keep the spec §8.5
  fallback.

## Details

### S1 Encoders (`spikes/raw/s1_1794053398923064606`, `s1_1794054026195797547`)
30 s per run, AnimActivity redrawing every vsync, `--max-fps=60`, Qualcomm `c2.qti.{avc,hevc}.encoder`.

| Streams @1280×800 | H.264 fps (each) | H.265 fps (each) |
|---|---|---|
| 1 | 59.8 | 59.8 |
| 2 | 59.8 ×2 | 59.8 ×2 |
| 3 | 59.8 ×3 | 59.8 ×3 |
| 4 | 59.7–59.8 | 59.7–59.8 |
| 5 | 59.4–59.8 | 59.6–59.8 |
| 6 | 56.5–56.8 | 55.2–55.4 |
| 3 @1920×1080 | 59.7–59.8 | 59.8 |

The first 6-stream H.264 run lost one stream to scrcpy's 5-port local range ("Server connection
failed" after 27183–27187 were taken). That was not an encoder failure. It was re-run with explicit
ports, and all six streams succeeded. Lantern uses one connection, so this limit doesn't apply.

### S6 Resize (`spikes/raw/s6_1794054085468710309`)
Own display and encoder through probe-shell. Each resize tears down and recreates the encoder.
20 reps × 4 sizes.

| Size | min | median | p95 (ms to first encoded frame) |
|---|---|---|---|
| 1280×800 | 95 | 146.5 | 163 |
| 1600×900 | 94 | 152 | 164 |
| 1000×1000 | 95 | 155.5 | 169 |
| 800×1280 | 131 | 144.5 | 158 |

The app redrew at the new size with a median of 108 ms (p95 121) for 55/80 resizes. In the other
25, the next resize arrived before the app drew, so they were coalesced. The pass bar of median
≤150 ms was met overall (all 80 events), but only narrowly.

### S2 Locked (`spikes/raw/s2_1794054293385894258`)
The phone has a **swipe keyguard, not a PIN**. A secure lock was not tested, because Sahaj's lock
settings were left untouched.
- Asleep or dozing with the keyguard showing: the probe display produced no frames, with either
  flag set. scrcpy's recording was empty too.
- Awake with the keyguard showing: normal flags gave 1 frame showing a lock-screen clock on the
  virtual display. `ALWAYS_UNLOCKED` gave 710 frames of the live probe app (`compare.png`). Stock
  scrcpy 5 (which sets `ALWAYS_UNLOCKED`) recorded 943 frames.

### S4 Focus (`spikes/raw/s4_1794054109225052713`)
Two displays, each running a probe task. KEYCODE_A was injected with display −1 (how unattributed
UHID or keyboard input arrives).

| Method | OWN_FOCUS off | OWN_FOCUS on |
|---|---|---|
| `setFocusedTask` | 10/10 | 10/10 |
| tap on target display | 10/10 | 10/10 |
| `IWindowManager.setFocusedDisplay` | rejected (InvocationTargetException) | rejected |

The host round trip, which includes two adb stdin hops, had a median of 94 ms (settask) and
120 ms (tap). On-device focus latency was not isolated. Plan wording asked for 20 reps per method;
40 per method were run across the two flag sets.

### S5 IME (`spikes/raw/s5_1794054133689564106`)
The probe IME (no input view) was set as the default for the test. The test committed the
Vietnamese and emoji string, then a composing `việ` and finish. The text arrived intact under all
three display IME policies. The inserted position follows the cursor; the cursor was mid-text
after an earlier tap, so concatenations look interleaved. Restore: the IME armed with the previous
IME and got no heartbeat, then wrote `default_input_method` back after about 5 s. Readback was
`com.android.inputmethod.latin/.LatinIME`.

### S3 Notifications (`spikes/raw/s3_1794054160321109372`)
The probe posted its own notification. The NotificationListener sent its `contentIntent` with
`ActivityOptions.setLaunchDisplayId(71)` and BAL mode ALLOWED. AnimActivity resumed on display 71
with `from=notif`.

### S7 Compatibility (`spikes/raw/s7_1794054216526443026/contact-sheet.png`)
Every launcher app was tested except PhoneDesk and the probe: the third-party apps, plus Jelly,
Settings and Messaging. That was 5 apps.

| App | Seen on Lantern display after 12 s |
|---|---|
| Zenless Zone Zero (`com.HoYoverse.Nap`) | HoYoverse splash |
| Diablo Immortal | Launcher art, "Updating files" |
| Jelly browser | Full Google page |
| Settings | Developer options page |
| Messaging | Conversation list |

None refused the display or rendered black. The games were not played past their splash or
update screens.

## Not verified (needs Sahaj or a later test)

- **Real gamepad button press** routed to the focused display (plan Task 4 Step 2).
- **Third-party app notification** click targeting. No third-party messaging app is installed.
- **Physical USB yank** IME restore. Only the heartbeat timeout was tested.
- **Text input into real apps** (browser address bar, messaging). Only the probe EditText was tested.
- **Secure (PIN) keyguard** with `ALWAYS_UNLOCKED`.
- **Encoder reconfigure without recreate**, and end-to-end latency versus scrcpy.

## Phone state after the spikes

Probe APK uninstalled, `/data/local/tmp` cleaned, no virtual displays left, keyguard dismissed
(it was a swipe lock). Diff of `before.json` → `after.json`:
- `input_methods_subtype_history`: the probe entry was added by the IME switch, then **restored** to
  the original value.
- `enabled_notification_listeners`: the probe left an empty key where none had existed; it was
  **deleted** again.
- System churn, left as is: `screen_brightness` 39→1 (adaptive brightness mode is on, so the
  system writes it), plus new empty `credential_service` and `uids_allowed_on_restricted_networks`
  keys created by the OS.
- `stay_on_while_plugged_in` = 15, the original value.
- Kept deliberately (approved): the four freeform/desktop flags are now 0.
