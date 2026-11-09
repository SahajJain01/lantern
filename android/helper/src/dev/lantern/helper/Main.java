package dev.lantern.helper;

import android.os.Build;

/** Helper entry point, run as the adb shell user via {@code app_process}. */
public final class Main {
    private Main() {
    }

    public static void main(String[] args) {
        String cmd = args.length > 0 ? args[0] : "";
        switch (cmd) {
            case "--version":
                System.out.println("lantern-helper " + Proto.VERSION + " sdk=" + Build.VERSION.SDK_INT);
                return;
            case "--ping":
                System.out.println("pong");
                return;
            case "--forget":
                if (args.length != 1) {
                    break;
                }
                System.exit(forget());
                return;
            case "--session-test":
                if (args.length != 2) {
                    break;
                }
                System.exit(sessionTest(Long.parseLong(args[1])));
                return;
            case "--display-test":
                if (args.length == 3 && args[2].equals("--recreate")) {
                    Encoder.forceRecreate = true; // measure the fallback resize path
                } else if (args.length != 2) {
                    break;
                }
                System.exit(displayTest(args[1]));
                return;
            case "serve":
                if (args.length != 3 || !args[2].matches("[0-9a-f]{32}")) {
                    break;
                }
                System.exit(serve(args[1], args[2]));
                return;
            default:
                break;
        }
        System.err.println("usage: dev.lantern.helper.Main serve <socket-name> <token: 32 hex>"
            + " | --version | --ping | --forget | --session-test <seconds> | --display-test <pkg> [--recreate]");
        System.exit(2);
    }

    /**
     * Undo-setup: restores a journal an earlier session left behind (only settings still holding
     * Lantern's value), then deletes it and Lantern's jars and APKs from {@code /data/local/tmp}.
     * Prints {@code forgot journal=<restored|none>}; exits 1 (journal kept) if the restore fails.
     */
    static int forget() {
        try {
            System.out.println(Cleanup.forget(JournalStore.device(Droid::log), new ShellSettings(),
                new java.io.File(JournalStore.PATH).getParentFile(), Droid::log));
            return 0;
        } catch (RuntimeException e) {
            System.err.println("forget failed: " + e);
            return 1;
        }
    }

    /** A helper nobody connects to must not linger. */
    static final long ACCEPT_TIMEOUT_MS = 30_000;

    /**
     * Serves exactly one lanternd connection on the abstract socket {@code name}. After a valid Hello
     * it starts Cleanup and the Session (so an unauthenticated client changes nothing), runs the
     * {@link Router} until EOF/Bye/error, then closes everything and exits 0.
     */
    private static int serve(String name, String token) {
        Droid.prepare();
        Droid.log("serve: socket=" + name + " pid=" + Droid.pid());
        final android.net.LocalSocket client;
        final boolean[] accepted = {false};
        try (android.net.LocalServerSocket server = new android.net.LocalServerSocket(name)) {
            Thread watchdog = new Thread(() -> {
                try {
                    Thread.sleep(ACCEPT_TIMEOUT_MS);
                } catch (InterruptedException e) {
                    return;
                }
                synchronized (accepted) {
                    if (!accepted[0]) {
                        Droid.log("serve: no client after " + ACCEPT_TIMEOUT_MS + " ms, exiting");
                        System.exit(3);
                    }
                }
            }, "lantern-accept-watchdog");
            watchdog.setDaemon(true);
            watchdog.start();
            System.out.println("listening " + name);
            System.out.flush();
            client = server.accept();
            synchronized (accepted) {
                accepted[0] = true;
            }
            watchdog.interrupt();
        } catch (java.io.IOException e) {
            Droid.log("serve: listen/accept on " + name + " failed: " + e);
            System.err.println("serve: " + e);
            return 1;
        }
        Droid.log("serve: client connected");
        try {
            java.io.DataInputStream in = new java.io.DataInputStream(
                new java.io.BufferedInputStream(client.getInputStream(), 1 << 16));
            java.io.DataOutputStream out = new java.io.DataOutputStream(
                new java.io.BufferedOutputStream(client.getOutputStream(), 1 << 16));
            final Router[] routerRef = {null};
            final Runnable[] linkRef = {null};
            final CompanionLink[] companionRef = {null};
            final boolean[] linked = {false};
            final String[] imeFailure = {null};
            // Set once the session is up: each display's IME policy follows it (AppDisplay.Policy.imePolicy).
            final java.util.concurrent.atomic.AtomicBoolean lanternIme =
                new java.util.concurrent.atomic.AtomicBoolean(false);
            Router router = new Router(in, out,
                (win, pkg, user, w, h, dpi, sink) -> AppDisplay.open(win, pkg, user, w, h, dpi, lanternIme.get(), sink),
                new Input(), Control.CODEC, token,
                () -> {
                    // After a valid hello only: an unauthenticated client never reaches the companion.
                    linked[0] = linkCompanion(routerRef, linkRef, companionRef);
                    // With the companion linked, the session also enables and selects the Lantern
                    // keyboard (journaled: restored at the end, by Cleanup, or by its watchdog). If
                    // that fails, the session goes on with STAY_ON only (Ruling 15); the link stays
                    // open for the catalogue. Each attempt spawns its own Cleanup guard.
                    ImeSetup.Started started;
                    try {
                        started = ImeSetup.start(linked[0], changes -> Session.start(new ShellSettings(),
                            new DevicePower(), Cleanup.spawnGuard(Droid::log), JournalStore.device(Droid::log),
                            changes), Droid::log);
                    } catch (RuntimeException e) {
                        closeLink(linkRef[0]);
                        throw e;
                    }
                    Session session = started.session;
                    imeFailure[0] = started.imeFailure;
                    lanternIme.set(started.imeSelected);
                    if (started.imeSelected) {
                        companionRef[0].setPrevIme(ImeSetup.prevIme(session.journal()));
                    }
                    return () -> {
                        try {
                            session.close();
                        } finally {
                            closeLink(linkRef[0]);
                        }
                    };
                },
                () -> {
                    // shutdown first: close() alone may not wake a thread blocked in write.
                    try {
                        client.shutdownInput();
                    } catch (java.io.IOException | RuntimeException ignored) {
                        // already closed
                    }
                    try {
                        client.shutdownOutput();
                    } catch (java.io.IOException | RuntimeException ignored) {
                        // already closed
                    }
                    client.close();
                },
                Droid::log);
            routerRef[0] = router;
            java.util.concurrent.ExecutorService imeSender = ImeSetup.Relay.executor(ImeSetup.RELAY_QUEUE);
            router.setExtras(new ImeSetup.Relay(m -> {
                CompanionLink l = companionRef[0];
                return l != null && l.send(m);
            }, Droid::log, imeSender));
            router.setLabels((pkg, user) -> {
                CompanionLink l = companionRef[0];
                return l == null ? null : l.label(pkg, user);
            });
            router.setOnSessionUp(() -> {
                router.sendSession(ImeSetup.companionState(linked[0], imeFailure[0]));
                // Only now (after the hello reply) is the catalogue asked for, so it goes straight
                // to lanternd instead of piling up in the router's pre-session buffer. A companion
                // that links later is asked right after its own hello.
                CompanionLink l = companionRef[0];
                if (l != null) {
                    l.startSync();
                }
            });
            try {
                router.run();
            } finally {
                imeSender.shutdownNow();
            }
        } catch (java.io.IOException e) {
            Droid.log("serve: " + e);
            return 1;
        } finally {
            try {
                client.close();
            } catch (java.io.IOException ignored) {
                // done anyway
            }
        }
        Droid.log("serve: exit");
        return 0;
    }

    /** How long a session start waits for the companion to link. */
    static final long COMPANION_CONNECT_MS = 3000;

    /**
     * Hands a pipe pair to the companion's provider and waits up to {@link #COMPANION_CONNECT_MS}
     * for its authenticated hello (false at once if the companion is not installed).
     * Its catalogue messages go to lanternd on stream 0. Never throws: without the companion the
     * session goes on.
     */
    private static boolean linkCompanion(Router[] routerRef, Runnable[] closeRef, CompanionLink[] linkRef) {
        try {
            HandoffListener listener = new HandoffListener();
            DeviceCompanion.Handoff handoff = new DeviceCompanion.Handoff(listener);
            CompanionLink link = new CompanionLink(listener, handoff, Control.CODEC, DeviceCompanion.hex(16), m -> {
                Router r = routerRef[0];
                if (r != null) {
                    r.sendSession(m);
                }
            }, Droid::log, DeviceUsers::managed);
            linkRef[0] = link;
            closeRef[0] = () -> {
                link.close();
                handoff.close();
            };
            long t0 = android.os.SystemClock.elapsedRealtime();
            boolean ok = link.connect(COMPANION_CONNECT_MS);
            Droid.log("serve: companion " + (ok ? "linked" : "unavailable") + " after "
                + (android.os.SystemClock.elapsedRealtime() - t0) + " ms");
            return ok;
        } catch (RuntimeException e) {
            Droid.log("serve: companion link setup failed: " + e);
            return false;
        }
    }

    private static void closeLink(Runnable close) {
        if (close != null) {
            try {
                close.run();
            } catch (RuntimeException e) {
                Droid.log("serve: companion link close failed: " + e);
            }
        }
    }

    /** Starts a session, holds it for {@code seconds}, closes it. Prints {@code pid=N} for kill tests. */
    private static int sessionTest(long seconds) {
        Droid.prepare();
        System.out.println("pid=" + Droid.pid());
        System.out.flush();
        Droid.log("session-test start pid=" + Droid.pid() + " seconds=" + seconds);
        try (Session s = Session.start(new ShellSettings(), new DevicePower(), Cleanup.spawnGuard(Droid::log),
                JournalStore.device(Droid::log))) {
            Droid.log("session started, journal=" + s.journal().toJson());
            System.out.println("session started");
            System.out.flush();
            Thread.sleep(seconds * 1000);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        } catch (RuntimeException e) {
            Droid.log("session-test failed: " + e);
            e.printStackTrace();
            return 1;
        }
        Droid.log("session closed");
        System.out.println("session closed");
        return 0;
    }

    static final String DISPLAY_TEST_OUT = "/data/local/tmp/lantern-display-test.h265";
    /** First ~2 s of the same capture: the host decoder fixture (Task 8). */
    static final String DISPLAY_TEST_HEAD = "/data/local/tmp/lantern-display-test-2s.h265";
    static final long HEAD_US = 2_000_000;

    /**
     * Opens {@code pkg} at 1280x800/160, records 8 s, then 1000x1000, 800x1280 and 1280x800 for 2 s
     * each, then closes. Raw Annex-B packets go to {@link #DISPLAY_TEST_OUT}. Polls taskVanished every
     * 250 ms and stops early with {@code app_exited} if the app quits. No Session: the panel stays on.
     */
    private static int displayTest(String pkg) {
        Droid.prepare();
        final long[] firstPts = {-1};
        final long[] packets = {0};
        final boolean[] headOpen = {true};
        try (java.io.FileOutputStream out = new java.io.FileOutputStream(DISPLAY_TEST_OUT);
             java.io.FileOutputStream head = new java.io.FileOutputStream(DISPLAY_TEST_HEAD)) {
            byte[][] buf = {new byte[1 << 16]};
            VideoSink sink = (win, ptsUs, config, key, data) -> {
                int n = data.remaining();
                if (buf[0].length < n) {
                    buf[0] = new byte[n];
                }
                data.get(buf[0], 0, n);
                if (!config && firstPts[0] < 0) {
                    firstPts[0] = ptsUs;
                }
                try {
                    out.write(buf[0], 0, n);
                    if (!config && ptsUs - firstPts[0] >= HEAD_US) {
                        headOpen[0] = false;
                    }
                    if (headOpen[0]) {
                        head.write(buf[0], 0, n);
                    }
                } catch (java.io.IOException e) {
                    throw new java.io.UncheckedIOException(e);
                }
                packets[0]++;
            };
            long t0 = android.os.SystemClock.elapsedRealtime();
            try (AppDisplay d = AppDisplay.open(1, pkg, 0, 1280, 800, 160, false, sink)) {
                d.setResizeListener((win, w, h) -> Droid.log("listener Resized win=" + win + " " + w + "x" + h));
                d.setStreamListener((win, reason) -> System.out.println("stream_failed win=" + win + " " + reason));
                System.out.println("display=" + d.displayId() + " task=" + d.taskId()
                    + " open_ms=" + (android.os.SystemClock.elapsedRealtime() - t0));
                System.out.flush();
                int[][] steps = {{1280, 800, 8000}, {1000, 1000, 2000}, {800, 1280, 2000}, {1280, 800, 2000}};
                for (int i = 0; i < steps.length; i++) {
                    if (i > 0) {
                        d.resize(steps[i][0], steps[i][1], 160);
                        System.out.println("resized " + steps[i][0] + "x" + steps[i][1]);
                    }
                    long end = android.os.SystemClock.elapsedRealtime() + steps[i][2];
                    while (android.os.SystemClock.elapsedRealtime() < end) {
                        Thread.sleep(250);
                        if (d.taskVanished()) {
                            String msg = "app_exited win=1 task=" + d.taskId() + " elapsed_ms="
                                + android.os.SystemClock.elapsedRealtime();
                            Droid.log(msg);
                            System.out.println(msg);
                            return 0;
                        }
                    }
                }
            }
        } catch (Exception e) {
            Droid.log("display-test failed: " + e);
            e.printStackTrace();
            return 1;
        } finally {
            System.out.println("packets=" + packets[0]);
        }
        return 0;
    }
}
