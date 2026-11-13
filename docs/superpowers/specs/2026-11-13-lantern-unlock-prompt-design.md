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
- **Unlock sequence (helper)**, display 0: wake (`KEYCODE_WAKEUP`), panel power on, dismiss the
  swipe layer (`KEYCODE_MENU` or a swipe-up), wait for the PIN pad, inject the digits as key
  events, `KEYCODE_ENTER`, wait up to 3 s for `isDeviceLocked()` to turn false, then panel off
  again. Reply `unlock_result{ok}`. One attempt per `unlock` message.
- **After success**: lanternd re-sends the last PC `clip` it held back, and a window whose open
  failed for the lock retries once.

## Left to the builder

- Exact waits and the PIN-pad readiness check (verify on both phones; a spike first is
  recommended: wake + dismiss + digits + Enter with the panel off, on the Motorola).
- How the window learns to retry its open (a new error string, like `NOT_ON_THIS_PHONE`).
- Whether `deviceLocked` is polled or read on demand.
- The "Try again" path after a wrong PIN (re-open the dialog from the failure notification).
