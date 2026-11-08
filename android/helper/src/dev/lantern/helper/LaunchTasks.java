package dev.lantern.helper;

import java.util.ArrayList;
import java.util.LinkedHashSet;
import java.util.List;
import java.util.Set;

/**
 * Which task a launch produced on a Lantern display, and cleanup when it went elsewhere. Pure policy
 * over {@link Tasks} (host tests fake it; {@link AppDisplay} backs it with IActivityTaskManager).
 *
 * <p>Before launching, {@link #record} notes every task of the package in that user on any display.
 * {@link #await} then waits for a task on the Lantern display. A task that was in the record is one
 * Android moved there (LauncherApps cannot ask for a new task; singleTask apps reuse theirs too):
 * {@link Found#moved}. If none appears in time, the launch may have landed on another display (the
 * phone's own screen), so every task of that package and user that is NOT in the record is removed;
 * tasks that existed before are never touched.
 */
final class LaunchTasks {
    private LaunchTasks() {
    }

    interface Tasks {
        /** Ids of every task whose app is {@code pkg} in {@code user}, on any display. */
        List<Integer> of(String pkg, int user);

        /** Ids of the tasks on {@code display}. */
        List<Integer> onDisplay(int display);

        boolean remove(int id);
    }

    interface Clock {
        long now();

        void sleep(long ms) throws InterruptedException;
    }

    static final long POLL_MS = 50;

    static final class Found {
        final int taskId;
        /** True when the task existed before the launch (Android moved it onto our display). */
        final boolean moved;

        Found(int taskId, boolean moved) {
            this.taskId = taskId;
            this.moved = moved;
        }
    }

    /** No task reached the display; {@link #removed} are the stray new tasks cleaned up elsewhere. */
    static final class Timeout extends Exception {
        final List<Integer> removed;

        Timeout(String msg, List<Integer> removed) {
            super(msg);
            this.removed = removed;
        }
    }

    static Set<Integer> record(Tasks t, String pkg, int user) {
        return new LinkedHashSet<>(t.of(pkg, user));
    }

    static Found await(Tasks t, String pkg, int user, int display, Set<Integer> before, long timeoutMs, Clock clock)
            throws Timeout, InterruptedException {
        long deadline = clock.now() + timeoutMs;
        while (true) {
            List<Integer> here = t.onDisplay(display);
            if (!here.isEmpty()) {
                int id = here.get(0);
                return new Found(id, before.contains(id));
            }
            if (clock.now() >= deadline) {
                break;
            }
            clock.sleep(POLL_MS);
        }
        List<Integer> removed = new ArrayList<>();
        for (int id : t.of(pkg, user)) {
            if (!before.contains(id) && t.remove(id)) {
                removed.add(id);
            }
        }
        throw new Timeout("no task on display " + display + " after " + timeoutMs + " ms"
            + (removed.isEmpty() ? "" : " (removed new task(s) " + removed + " of " + pkg + " that landed elsewhere)"),
            removed);
    }
}
