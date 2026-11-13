# Lantern: several phones at once, per-phone folders, autoconnect (design)

Status: draft for Sahaj's approval, 2026-11-14 (builder check done; his answers folded in). It builds on the main design
(`2026-11-07-lantern-design.md`), Plan 3 (`plans/2026-11-10-lantern-phones-and-tray.md`: phone
names, tray, `settings.toml`, sticky Disconnect) and Plan 4a (unlock prompt). Read
`docs/HANDOFF.md` and `docs/protocol.md` first.

## Intent

Sahaj uses two phones with Lantern: the Xiaomi 11T Pro and the Motorola edge 50 pro, which has
a work profile and a PIN. Lantern was built for one phone at a time:

- One session runs; the first phone online wins.
- All phones' apps share one "Phone" menu folder, and an app on both phones is one entry.
- Disconnect is forgotten when the phone is unplugged.

He wants three things:

1. **Several phones connected at once.** Apps from each connected phone open side by side as
   their own windows.
2. **He decides which phones connect.** An *autoconnect* setting. While it is off, plugging a
   phone in never connects it by itself; Lantern asks every time. While it is on, a plugged-in
   phone connects by itself.
3. **The tray as the phone switchboard, and a menu that shows phones clearly.**
   - The tray lists the plugged-in phones. Each phone is a connect/disconnect toggle,
     replacing today's separate Connect and Disconnect buttons.
   - The app launcher has one folder per phone.

## Goals

- **G1. Per-phone launcher folders.** One submenu per phone, named after it ("Xiaomi 11T Pro",
  "edge 50 pro"), under the existing "Phone" folder. Each folder holds that phone's apps, work
  profile apps included, with the existing "(work)" suffix.
  - An app on both phones gets one entry in each folder.
  - Each entry opens on its own phone.
- **G2. Entries are never removed because a phone disconnected or was unplugged.** Pinned
  launchers and favourites keep working.
  - Clicking an entry of a phone that isn't connected shows "Connect your <phone>", with a
    **Connect** button.
  - Clicking that button connects the phone, then opens the app once the session is ready, once
    only.
  - Entries still go away when that phone's full app sync says the app is gone (today's rule),
    or after Undo phone setup.
- **G3. Tray phone list.** One row per plugged-in phone, a checkmark toggle named after the
  phone. Checked means connected; checking connects and unchecking disconnects.
  - Per-phone state that needs attention (connecting, retrying, locked with **Unlock…**,
    companion problems) shows next to or under that phone's row.
- **G4. Autoconnect setting.** `autoconnect` in `settings.toml`, shown as a checkbox in the tray
  next to the other settings.
  - **Off:** every time a phone is plugged in (comes online in adb), Lantern shows a
    notification "<phone> is plugged in" with a **Connect** button. If it is ignored, the phone
    waits in the tray, unchecked.
  - **On:** a plugged-in phone connects by itself, unless Sahaj switched it off (G5).
  - A phone Lantern has never set up connects through the same paths. The first connection
    installs the companion as it does today.
- **G5. Disconnect is remembered** per phone, across unplugging and lanternd restarts, until
  that phone is switched on again. This only matters with autoconnect on, because with it off
  nothing connects without a click.
- **G6. Several phones live at once.** Every checked phone has its own session: helper,
  windows, keyboard, panel-off and settings journal, exactly as one phone has today.
  - Windows from different phones are separate in KDE: their own taskbar groups, titles and
    remembered sizes.
- **G7. Clipboard with several phones.**
  - A copy on the PC goes to **every** connected phone.
  - A copy on any phone goes to the PC, and from there to the other connected phones.
  - No phone gets its own copy echoed back.
  - A locked phone holds the PC's copy, as Plan 4a does, per phone.
- **G8. The unlock prompt works per phone.** The notification and the tray Unlock row name
  their phone, and the PIN goes only to that phone (Plan 4a's serial pinning, unchanged).

## Non-goals

- **Moving a window between phones,** or one app window showing two phones.
- **A dedicated settings window.** The tray stays the settings home.
- **Phones over Wi-Fi adb.** USB only, as today. A Wi-Fi phone would just be another serial,
  but it is not tested or designed for.
- **Per-phone autoconnect.** One global setting; the per-phone memory is "switched off" (G5).
- **Limiting how many phones connect.** No cap; two is the tested case.

## Anti-goals (a failure even if it technically works)

- **A phone connects without his say-so while autoconnect is off.** Its panel turning off and
  its keyboard switching are exactly what he wants to control.
- **A PIN, a keystroke or a clipboard copy meant for one phone reaches another**, except PC
  copies, which go to all connected phones by design.
- **Removing a launcher entry because its phone went away.** That breaks pinned apps.
- **One phone's session failing, retrying or being undone disturbs another phone's live
  session or its windows.**
- **Any phone setting left changed on any phone** after its session ends: the restore rules of
  Plans 1–3 apply per phone.

## Constraints

- **Protocol stays v1** between helper and lanternd: no change is needed there, since each
  phone keeps its own helper link.
  - The window→lanternd `open` may gain one optional field naming the phone, omitted when
    absent.
  - Existing bytes and goldens stay unchanged.
- **Every adb call names its serial and has a timeout** (existing rule).
- **Only `.desktop` files with `X-Lantern-Serial=` are Lantern's** (existing rule). The
  per-phone layout must keep that marker.
- **The `LANTERN_SERIAL` environment variable stays** as a test override: when set, lanternd
  serves only that phone. The device suite depends on it.
- **The tray is optional:** without a session bus, lanternd still runs.
  - Autoconnect-off prompts need the notification server.
  - If there is none, a newly plugged phone just waits. `lantern connect <serial>` connects it
    from a terminal.

## Approaches considered

- **A. One lanternd running one session per phone** (chosen by Sahaj, 2026-11-14).
  - The run loop keeps a map from serial to that phone's session state, instead of today's one
    `active` / `starting` / `paused` set.
  - Tray, menu catalogue, clipboard and unlock prompt stay single owners inside one process,
    and they learn which phone each thing belongs to.
  - Trade-off: the biggest rewrite of the daemon core (`daemon/mod.rs` drive loop, the IPC hub,
    the published `View`). Coordination stays simple because everything is in one place.
- **B. One lanternd per phone plus a coordinator process.**
  - Isolates phones by process.
  - Trade-off: the PC clipboard, the tray and the launcher folders all need a single owner. So
    the coordinator must own all of them, plus messaging between the processes. Rejected: more
    moving parts and more ways for state to drift.
- **C. Keep one phone at a time and make switching quick.** Rejected: it isn't what he asked
  for.

Order, chosen by Sahaj ("go Approach A" after the two-plan proposal):

- **Plan 4b** delivers G1–G5 with one phone live at a time. Checking a phone in the tray while
  another is live switches the other off, and the tray says so.
- **Plan 4c** delivers G6–G8: several phones live at once.

Plan 4b is useful by itself, and it tests the new tray and menu before the core changes.

## Decided details

### Launcher (G1, G2)

- Today's single "Phone" folder becomes a parent, with one submenu per phone named by its
  recorded name (`companion::Names`, from `settings get global device_name`), else the serial.
  It is built with a per-phone `.directory` file and a per-phone category in Lantern's merged
  menu file. *(Builder: choose the category naming, e.g. a sanitized form of the serial.)*
- **One `.desktop` file per (phone, app, Android user).** Today's shared files with
  `X-Lantern-Serials=` lists are migrated once to per-phone files when lanternd starts.
  - Its `Exec` names the phone.
  - Its `StartupWMClass` and the window's app_id include a per-phone part, so KDE groups each
    phone's windows on their own.
  - Remembered window sizes (`windows.json`) are keyed per phone.
  - Both the migration and the new key scheme are the builder's to design, with tests.
- **This renames every Lantern menu file once.** Apps Sahaj pinned to the taskbar or favourites
  before the change need re-pinning once. After it, names are stable.
  - A sensible builder may keep the old file names for one designated phone to avoid that.
    *(Builder's call; say which in the plan.)*
- **`lantern open`** takes the phone as an option.
  - An entry without it, an old pin or a hand-typed command, opens on the only connected phone
    that has the app.
  - If two connected phones have it, it opens on the most recently connected one.
  - If none is connected, it shows today's "Connect your <phone>" for the phone that was
    connected last.

### Tray (G3, G4)

- **Rows, top to bottom:**
  1. One checkmark row per plugged-in phone ("Xiaomi 11T Pro", "edge 50 pro").
  2. Under a phone that needs attention, an indented status line and its action:
     - "Connecting…" or "Reconnecting…";
     - "Locked" with **Unlock edge 50 pro…**;
     - companion install failed;
     - keyboard off.
  3. Separator.
  4. The settings checkboxes: Clipboard sync, Lantern keyboard, **Autoconnect** (new), Start
     at login.
  5. Separator.
  6. "Remove Lantern from a phone…", a submenu of known phones that replaces today's single
     Undo row and keeps its kdialog confirm pinned to the phone named (Plan 3 R9).
  7. Quit.
- **Phones that are known but unplugged** are not listed in the tray. Their launcher folders
  stay (G2).
- **With no phone plugged in:** one label, "No phone plugged in".
- **Exact wording and row ids** are the builder's. Keep the id stability rule: tests and
  device item 20 click rows by id.

### Connect rules (G4, G5)

- **Remembering "off":** a per-phone `off` flag in the phone's existing state record
  (`~/.local/state/lantern/devices/<serial>.json`).
  - Set by unchecking the phone or by Undo.
  - Cleared by checking it, by the Connect button of either notification, or by `lantern
    connect <serial>`.
- **When a phone comes online:**

  | Autoconnect | Phone's `off` flag | What happens |
  |---|---|---|
  | off | any | Notification "<phone> is plugged in" with **Connect** (one per plug-in). The phone waits, unchecked. |
  | on | not set | Connects. |
  | on | set | Stays unchecked, no notification. *(My call: he switched it off on purpose.)* |

- **Autoconnect defaults to off,** matching "doesn't get connected unless the toggle is on".
  *(My call: an existing install will start asking after the update instead of connecting by
  itself.)*
- **Switching autoconnect on** doesn't connect phones already plugged in and unchecked. It
  applies to the next plug-in. *(My call: a settings click shouldn't flip phone screens off.)*
- **"Plugged in" means a real plug-in, not a restart or a blip** (Sahaj, 2026-11-14):
  - Each phone's record keeps a `live` flag. It is set when a session becomes ready and
    cleared on a real unplug, an uncheck, Undo or Quit.
  - **lanternd starting** (login, `build.py install`, crash restart) reconnects the phones whose
    `live` flag is set, with no prompt.
  - **A connected phone that drops out of adb and comes back within about 10 s** reconnects with
    no prompt ("Reconnecting…" in the tray). Any longer absence counts as an unplug: the flag is
    cleared and the next plug-in follows the table above.
  - **`LANTERN_SERIAL`** (tests) bypasses autoconnect and the `off` flag for the phone it names,
    so `build.py install && device_test.py` starts that phone's session by itself.
- **The Connect button for a phone that isn't plugged in** (Sahaj, 2026-11-14): the notice
  reads "Plug in your <phone>". Lantern remembers the request for about 2 minutes. If that phone
  is plugged in within the window, it connects and opens the app even with autoconnect off.
  - The request is lanternd's, not the `lantern open` process's, which exits as today.
  - A later request for the same phone replaces the earlier one.
- **Plan 4b, one phone live** (builder's calls):
  - If two phones qualify at once, the first one online connects.
  - When the live phone goes away, a waiting phone doesn't start by itself.
  - Checking B while A is live switches A off for now but doesn't set A's `off` flag, because
    Sahaj didn't choose it.
- **Undo** sets the `off` flag and keeps the phone's record, so a replug with autoconnect on
  doesn't reinstall the companion. The phone stays in "Remove Lantern from a phone…". Checking
  it later sets Lantern up again. *(Builder's call; today Undo deletes the record.)*
- **`lantern disconnect`** sets `off` exactly as unchecking does.
- **`lantern connect` / `lantern disconnect`** take an optional serial. Without one, they act
  on the only plugged-in phone, or fail with a list when there are several.
- **`lantern status`** reports every phone. Its JSON shape changes; the device suite's readers
  are updated with it.

### Several sessions (G6–G8), Plan 4c

- **Each phone has its own session.** That means its own helper (with its own adb forward
  port), retry backoff, companion state, lock state, journal and restore.
  - A failed start or a helper death on one phone retries that phone only.
- **Window IPC:** each `lantern open` connects to lanternd as today and names its phone. The
  daemon routes the window to that phone's session.
  - Window ids stay unique across phones.
  - Single-instance focus keys on phone + package + user.
- **Clipboard (G7):** the PC clipboard has one watcher, which sends each PC copy to every live
  session. Echo suppression is per phone. A phone's copy is set on the PC, and the PC change
  then fans out to the other phones, never back to its source.
- **Unlock (G8):** each phone has its own lock stretch and notification. Only one PIN dialog
  is open at a time, across phones. The Plan 4a serial pinning stays as it is.
- **Undo phone setup** acts on the phone named, without touching other live sessions.
- **Plan 4b's "checking one phone switches the other off" rule is removed.**

## Left to the builder

- **Data structures:** the per-phone session map, the IPC hub split, and the `View` shape
  (a list of per-phone views).
- **Menu file and category naming,** and the one-time migration of shared entries, with tests
  that a non-Lantern file is never touched.
- **app_id and `windows.json` key formats,** and whether to keep old names for one phone.
- **Exact tray row ids and wordings,** within the order above.
- **How `lantern open` without a phone resolves,** within the rule above.
- **Device checks:**
  - **4b:** autoconnect off asks on each plug-in; on connects; "off" survives a replug and a
    lanternd restart; per-phone folders; a Connect from a disconnected phone's entry opens the
    app.
  - **4c:** both phones live with a window from each; a PC copy reaches both; a phone copy
    reaches the PC and the other phone; locking one phone doesn't affect the other's windows;
    unplugging one leaves the other's session and windows running.
