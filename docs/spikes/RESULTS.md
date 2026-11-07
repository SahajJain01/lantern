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
| 1 Encoder concurrency | **Pass** | 6 × 1280×800 sustained ~55–57 fps for 30 s with light 2D content; up to 5: ~59.7 fps; 3 @1080p: 59.8 fps. Whole pipeline measured, single run each, no thermal soak |
| 6 Resize cost | **Pass (borderline)** | Median 144–156 ms resize → first encoded frame (80/80, HEVC, full encoder recreate). App redraws ~47 ms *before* the encoder — encoder recreate is the bottleneck |
| 2 Locked phone | **Partial** | Only after the phone is woken: normal flags → keyguard clock (1 frame); ALWAYS_UNLOCKED → live app. Asleep → no frames. Wake → keyguard → panel-off, and >8 s, untested |
| 4 Focus routing | **Routing pass; latency not isolated** | `setFocusedTask` and tap both routed 20/20 per flag set. `setFocusedDisplay` **not tested** (probe bug). Not tested with ALWAYS_UNLOCKED |
| 5 Hidden IME | **Partial pass** | Unicode commit + compose exact through one input connection. Policy effect on rebind/visibility untested (no rebind happened). Restore wrote the setting back after 5 s; actual bound IME not checked |
| 3 Notification click → display | **Weak pass** | Own PendingIntent, sender in foreground, new task forced — the easy case. Background listener firing another app's intent is untested |
| 7 App compatibility | **No refusals seen** | 5/5 launchable apps rendered (2 games at splash/updating screens) |

### Design impact

- **Codec:** both held ~60 fps up to 5 streams; at 6, H.264 56.6 vs H.265 55.3 (separate runs, not a
  fair comparison). S1 does not favour either codec; choose HEVC on bandwidth grounds if at all. Light
  multitasking (3–4 windows) has clear headroom; a hard cap needs a longer, heavier, thermal test.
- **Display flags:** use scrcpy 5's set, **including `ALWAYS_UNLOCKED`** (the shell may set it on this
  ROM) and `OWN_FOCUS`. Without `ALWAYS_UNLOCKED`, a Lantern display shows the lock screen.
- **The phone must stay awake:** a dozing/asleep phone produced no frames even with
  `ALWAYS_UNLOCKED`. This confirms spec §8.2: keep the phone awake and power the panel off (scrcpy
  `--turn-screen-off` style). Never send the sleep key.
- **Resize:** ~150 ms per resize, dominated by encoder teardown/recreate (the app had already
  redrawn ~47 ms earlier). Testing encoder reuse/reconfigure is the **main lever** and comes before
  relying on debouncing. The app was also paused/resumed on every resize despite handling config
  changes — matters for games.
- **Focus:** `IActivityTaskManager.setFocusedTask(taskId)` works and avoids injected touches (side
  effects not observed visually). `setFocusedDisplay` still needs a proper test via the working
  WindowManager wrapper.
- **IME:** the hidden companion IME delivers exact Unicode/compose text; keep the design. Still to
  prove: which display policy behaves best across a rebind, and that restore switches the *bound*
  keyboard, not just the setting.
- **Notifications:** only the easy case is proven. A background listener firing another app's
  PendingIntent is untested; keep the spec §8.5 fallback.
- **Locked phone:** the helper must wake the phone before creating displays; the wake → keyguard →
  panel-off sequence needs its own test.

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

App redraw at the new size: measured for the last 54/80 resizes (the first 26 fell before the
retained logcat window — buffer wrap, not coalescing). All 54 redrew; median 108.5 ms, p95 121
(computed ad hoc, not by `s6_resize.py`). In every match the app drew 37–56 ms *before* the first
encoded frame. Rep 1 includes a no-op 1280×800→1280×800 resize. Codec: HEVC. The pass bar (median
≤150 ms) was met, narrowly.

### S6 follow-up: encoder reuse vs recreate (helper Task 4, 2026-11-07)
`Main --display-test com.android.settings` with the production `AppDisplay`/`Encoder`
(HEVC `c2.qti.hevc.encoder`, display flags per spec §9b). Each run resizes 1280×800 → 1000×1000
(encoder 992×992) → 800×1280 → 1280×800. `resize_ms` = from `Encoder.reconfigure` start to the
first encoded non-config frame at the new size (the spike's metric also included the
`setSurface(null)` call, which is negligible).

| Path | Samples | min | median | max (ms) |
|---|---|---|---|---|
| Reuse: `stop(); configure(); createInputSurface(); start()` on the same `MediaCodec` | 12 (4 runs) | 102 | 112.5 | 123 |
| Recreate: `release()` + `createByCodecName` (forced with `--recreate`) | 9 (3 runs) | 128 | 134 | 150 |

Reuse never failed on this phone, so recreate is only the fallback. Reuse saves ~20 ms per resize.
The synchronous part on the caller's thread (`setup_ms`) was 51–56 ms with reuse; the rest is the
app redrawing and the first frame encoding. Debounce drags regardless.

### S2 Locked (`spikes/raw/s2_1794054293385894258`)
The phone has a **swipe keyguard, not a PIN**. A secure lock was not tested, because Sahaj's lock
settings were left untouched.
- Asleep or dozing with the keyguard showing: the probe display produced no frames, with either
  flag set. scrcpy's recording was empty too.
- Awake with the keyguard showing (ad-hoc commands, **not** in committed `s2_locked.py`; keyguard and
  power state not captured during these runs): normal flags gave 1 frame showing a lock-screen clock;
  `ALWAYS_UNLOCKED` gave 710 frames of the live probe app in 5.85 s (`compare.png`, uncapped fps).
  Stock scrcpy 5 (which sets `ALWAYS_UNLOCKED`) recorded 943 frames of the app.
- Spec §9.2's real case (phone asleep and locked at plug-in → wake → keyguard → panel off, sustained)
  is **untested**.

### S4 Focus (`spikes/raw/s4_1794054109225052713`)
Two displays, each running a probe task. KEYCODE_A was injected with display −1 (how unattributed
UHID or keyboard input arrives).

| Method | OWN_FOCUS off | OWN_FOCUS on |
|---|---|---|
| `setFocusedTask` | 10/10 | 10/10 |
| tap on target display | 10/10 | 10/10 |
| `IWindowManager.setFocusedDisplay` | not tested — probe failed fetching the service (`ApplicationSharedMemory not initialized`) | not tested |

The host round trip, which includes two adb stdin hops, had a median of 94 ms (settask) and
120 ms (tap); the plan's ≤100 ms on-device bar was **not measured**. "OWN_FOCUS on" also set
DEVICE_DISPLAY_GROUP, so those two flags are confounded; ALWAYS_UNLOCKED was not included; both
displays ran the same app process. 20 reps per method per flag set (40 total).

### S5 IME (`spikes/raw/s5_1794054133689564106`)
The probe IME (no input view) was set as the default for the test. The test committed the
Vietnamese and emoji string, then a composing `việ` and finish. Every insert landed at the cursor
(offset 3, after an earlier tap) and is byte-exact (checked by decomposition; the script's own check
is a weak substring match). Only one `ime_start` occurred, before the policy changes, so all three
policies reused one input connection — the policies were **not** really compared, and the plan's
display-0 screenshot was not taken. Restore: with no heartbeat, the IME wrote `default_input_method`
back after ~5 s (setting readback only; the bound IME wasn't checked, and the driver's `finally`
would mask a failure).

### S3 Notifications (`spikes/raw/s3_1794054160321109372`)
The probe posted its own notification. The NotificationListener sent its `contentIntent` with
`ActivityOptions.setLaunchDisplayId(71)` and BAL mode ALLOWED. AnimActivity resumed on display 71
with `from=notif`. Caveats: the same app created and sent the intent; the probe also had a visible,
resumed activity on display 0 (task 86), so background-launch rules were trivially met; the intent
forced a new task.

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

## Follow-up tests with Sahaj present (2026-11-07 evening)

**S2 locked phone, real case: PASS** (`spikes/raw/s2real_1794057894`). The phone was asleep with the
keyguard showing (`Dozing`, `showing=true`, screen off). It was woken with the power key, so the
keyguard was still showing. Then stock scrcpy 5 (`ALWAYS_UNLOCKED`) ran with `--new-display` and
`--turn-screen-off` for 30 s: 3,641 frames in 29.8 s. Frames at 2, 15 and 29 s all show the live
probe app with its counter advancing (238 → 1830 → 3543), and the keyguard stayed up throughout.
**The helper must wake the phone but does not need to unlock it.** The 2 s frame shows a
system taskbar on the virtual display, which comes from scrcpy's system-decorations flag. Lantern
must leave `SHOULD_SHOW_SYSTEM_DECORATIONS` unset.

**S4 real gamepad, GameSir over scrcpy UHID: delivered, but pinned to a display**
(`spikes/raw/s4pad_1794057457915006316`). 137 button presses (keycodes 96/97/100/102/103/62/23)
reached the probe as gamepad-source key events. All of them went to the display scrcpy was
mirroring, including after `setFocusedTask` moved focus to the other display (`FocusedDisplayId:
89` confirmed). The cause is in scrcpy 5's `UhidManager`: on API 35+ it calls
`InputManager.addUniqueIdAssociationByPort(port, displayUniqueId)`, which pins the UHID device to
one display. **Design impact:** Lantern should re-associate each pad's input port to the active
window's display when PC focus changes. That is more deterministic than following focus.

**S5 IME, physical USB unplug: setting restored by the phone alone**
(`spikes/raw/s5yank_1794057546594114700`). The test was armed and heartbeats ran for about 287 s.
Sahaj then unplugged the cable for 10.1 s. The device log shows `ime_restored ok=true` about 5 s
after the last heartbeat, while unplugged, and `default_input_method` read `LatinIME` right after
replug. The script's bound-IME check matched nothing because Android 17's `dumpsys input_method`
has different field names (it has `imeToken=… [component]`), so its "pass=false" is a check bug.
The bound IME was not independently confirmed before cleanup ran.

**Host finding, not about the phone:** the GameSir Tegenaria Lite (3537:1093) re-enumerates on USB
every 2–3 s whenever no program has its evdev node open. Autosuspend is already off
(`power/control=on`). While a reader held it open for 15 s there were 0 disconnects, and the
disconnects resumed right after it was released. Windows keeps it stable because its HID game
stack always reads controllers. **Design impact:** `lanternd` should open gamepads for the whole
session; that also prevents this churn.

## Not verified (needs Sahaj or a later test)

- **Gamepad re-association** to a different display (Lantern's own UHID path); scrcpy pins it.
- **Third-party app notification** click targeting. No third-party messaging app is installed.
- **Bound IME** after a physical yank (setting restore is proven; see follow-ups).
- **Text input into real apps** (browser address bar, messaging). Only the probe EditText was tested.
- **Secure (PIN) keyguard** with `ALWAYS_UNLOCKED` (the swipe-keyguard real case passes).
- **Encoder reconfigure without recreate**, and end-to-end latency versus scrcpy.

## Phone state after the spikes

Probe APK uninstalled, `/data/local/tmp` cleaned, no virtual displays left, keyguard dismissed
(it was a swipe lock). Final diff `before.json` → `after2.json`:
- `input_methods_subtype_history`: the probe entry was added by the IME switch, then **restored** to
  the original value.
- `enabled_notification_listeners`: after the probe's listener was granted and revoked, this key
  holds an empty value where none existed before. I deleted it, and the OS recreated it as empty
  within minutes. An empty list behaves the same as an absent one: no listeners are enabled.
- System churn, left as is: `screen_brightness` 39→1 (adaptive brightness mode is on, so the
  system writes it), plus new empty `credential_service` and `uids_allowed_on_restricted_networks`
  keys created by the OS.
- `stay_on_while_plugged_in` = 15, the original value.
- Kept deliberately (approved): the four freeform/desktop flags are now 0.
- Seen in the S7 Settings screenshot: Developer options shows **"Enable desktop experience features"**
  on (greyed out). That is separate from the four reset flags and, with no reboot, may still affect
  secondary-display windowing.

## Test-method caveats
- The probe's AnimActivity (task 86) stayed visible with KEEP_SCREEN_ON on display 0 for all spikes,
  affecting wakefulness (S1, S7), background-launch rights (S3) and IME targets (S5).
- No driver checked keyguard/sleep state at start (plan Review Focus 2); S1's 59.8 fps of animated
  output shows it was not an issue there.
- Two earlier S7 runs produced no rows (a regex bug, since fixed).
