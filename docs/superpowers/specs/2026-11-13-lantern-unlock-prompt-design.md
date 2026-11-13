# Lantern — unlock a PIN-locked phone from the PC (design)

Date: 2026-11-13 · Status: draft for Sahaj's approval · Builds on Plan 3 (tray, settings, notifications)

## Intent

On a phone with a PIN lock screen, Lantern's windows still render (ALWAYS_UNLOCKED displays),
but Android blocks anything that needs an unlocked user: every clipboard read returns nothing
(field finding on the Motorola, 2026-11-13), and after a reboot no app starts at all until the
PIN has been entered once. Today this fails silently. Sahaj wants Lantern to notice, tell him,
and let him unlock the phone from the PC without picking it up.

## Goals

1. **Notice when the lock gets in the way**, and only then:
   - a PC copy that can't land on the phone (the phone is locked when lanternd forwards `clip`);
   - a phone copy that reads back empty while the phone is locked;
   - an app that fails to open while the phone is locked.
2. **One Lantern notification** per locked stretch: "Unlock your <phone name>" with an action
   "Enter PIN…". Clicking it opens a KDE password dialog.
3. **Unlock with the typed PIN**, then **check it worked** (`deviceLocked=0`) and turn the panel
   back off. On success, finish what was blocked: re-send the last PC copy, retry the failed open.
4. **Wrong PIN** gives "That PIN didn't unlock <phone name>", with a way to try again.

## Non-goals

- Patterns, passwords with letters beyond what `input text` can type, biometrics.
- Mirroring the phone's lock screen into a window (approach 2; later, if blind typing proves
  unreliable).
- Unlocking a work profile with its own separate challenge (the Motorola's is unified).
- Prompting at connect time when nothing is blocked.

## Anti-goals (failure even if it works)

- The PIN is stored anywhere (disk, settings, logs, journal, process args, shell history,
  `logcat`) or crosses the USB link in any reusable form. It exists in memory for one attempt.
- Lantern burns PIN attempts on its own: no automatic retries, and never more than one attempt
  per dialog submit.
- The panel is left on, or the phone is left on a half-typed lock screen, after an attempt.
- A notification storm while the phone stays locked.

## Approaches considered

1. **PIN dialog on the PC, typed blind into the lock screen** (chosen by Sahaj). The helper
   wakes the lock screen on display 0, opens the PIN pad, types the digits, presses Enter,
   re-reads `deviceLocked`, and turns the panel off again. What Sahaj asked for; reuses the
   input-injection path the helper already has. Risk: typing blind, so an unexpected screen gets
   the digits; the re-check catches it and reports failure.
2. Mirror display 0 into a Lantern window and let him type into Android's real keypad. Works
   for any lock type and is visible, but it's a new kind of window and not the asked-for popup.
3. Both, dialog first with a "Show phone screen" fallback. Deferred until approach 1 is tried on
   both phones.

## How (decided)

- **Detection** is the helper's: it already runs as shell and can read the lock state
  (`KeyguardManager.isDeviceLocked()` for user 0, or `dumpsys trust`), and it sees the three
  events above. It reports the lock to lanternd with a new optional session message (protocol
  stays v1: a new message type `locked{reason}` helper → daemon, reasons `clipboard`, `open`).
  An open that fails while locked gets its error from the helper as today, plus `locked`.
- **Notification** comes from lanternd (the tray process owns D-Bus): freedesktop
  `org.freedesktop.Notifications.Notify` with an action, because `notify-send` can't wait for
  a click. One per locked stretch; the stretch ends when the helper reports `unlocked`.
- **Dialog**: `kdialog --password "Enter the PIN of <phone name>"`, spawned through
  `desktop::spawn`. Its stdout (the PIN) is read into memory, validated as 4–16 digits, and sent
  to the helper in one new host → helper message `unlock{pin}` on stream 0. lanternd and the
  helper must never log it; the message type is excluded from every log path (as `ime` and
  `clip` text already are).
- **Unlock sequence (helper)**, every event targeted at display 0 (see the spike): wake
  (`KEYCODE_WAKEUP`), swipe up (ends a screensaver and opens the PIN pad), check display 0's
  focus is the keyguard, inject the digits and `KEYCODE_ENTER` as key events, wait up to 3 s for
  `isDeviceLocked()` to turn false, then put the panel back off if Lantern had it off. Reply
  `unlock_result{ok}` (or `reason: "lock screen not reached"`). One attempt per `unlock`.
- **After success**: lanternd re-sends the last PC `clip` it held back, and a window whose open
  failed for the lock retries once.

## Spike result (2026-11-13, Motorola edge 50 pro, Lantern session live, panel off)

The blind PIN path works: **deviceLocked 1 → 0 in 2.3 s** after the digits, with Lantern's
session (and its Windows App window) running throughout. What it took, and what broke first:

- **Every injected key must target display 0** (`input -d 0`, i.e. `KeyEvent.setDisplayId(0)`).
  An untargeted key goes to the top-focused display, which during a session is usually a Lantern
  window: the first attempt typed the digits into Windows App's home screen instead of the lock
  screen. The helper must inject with an explicit display id, never the default.
- **The phone may be in its screensaver** (Motorola: `screensaver_enabled=1`, activate while
  charging; display 0 focus was `com.google.android.deskclock/…DreamActivity`, wakefulness
  `Dreaming`). `KEYCODE_WAKEUP` alone did not end it; `cmd dreams stop-dreaming` needs root. A
  swipe-up on display 0 after the wake-up ended the dream and opened the PIN pad: display 0's
  focus became `NotificationShade`.
- **Check display 0's focus right before typing** and type only when it is the keyguard
  (`NotificationShade`); otherwise type nothing and report "couldn't reach the lock screen".
  After a correct PIN the focus becomes the launcher.
- `screencap` of display 0 shows black while the keyguard/dream is up (secure layers), so it
  can't be used to check the screen.
- Sequence used: `input -d 0 keyevent KEYCODE_WAKEUP`, 0.5 s, `input -d 0 swipe 600 2200 600 600
  200`, 1 s, focus check, `input -d 0 text <pin>`, `input -d 0 keyevent KEYCODE_ENTER`, poll
  `deviceLocked` every 0.5 s. The real build injects KeyEvents from the helper (no PIN in any
  process's arguments, unlike this spike's `input text`).
- Not yet verified: the Xiaomi (swipe keyguard only, so nothing to unlock), whether the panel
  physically lit during the attempt, and a wrong PIN.

## Left to the builder

- Exact waits; a wrong-PIN run on the Motorola.
- How the window learns to retry its open (a new error string, like `NOT_ON_THIS_PHONE`).
- Whether `deviceLocked` is polled or read on demand.
- The "Try again" path after a wrong PIN (re-open the dialog from the failure notification).
