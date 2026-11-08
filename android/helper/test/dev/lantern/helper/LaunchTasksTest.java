package dev.lantern.helper;

import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Set;

/**
 * Host-JVM tests for {@link LaunchTasks}: the pre-launch task record, the wait for a task on the
 * Lantern display, cleanup of stray new tasks on timeout, and the "moved" flag. Fakes only.
 */
public final class LaunchTasksTest {
    /** A phone's task list: id -> {pkg, user, display}. */
    static final class Fake implements LaunchTasks.Tasks {
        final Map<Integer, Object[]> tasks = new LinkedHashMap<>();
        final List<String> calls = new ArrayList<>();
        /** Polls until a task appears (-1: never); the launched task then lands per {@link #land}. */
        int appearAfterPolls = -1;
        Object[] land;
        int landId;
        int polls;

        Fake task(int id, String pkg, int user, int display) {
            tasks.put(id, new Object[] {pkg, user, display});
            return this;
        }

        @Override
        public List<Integer> of(String pkg, int user) {
            calls.add("of " + pkg + " " + user);
            List<Integer> ids = new ArrayList<>();
            for (Map.Entry<Integer, Object[]> e : tasks.entrySet()) {
                if (e.getValue()[0].equals(pkg) && (int) e.getValue()[1] == user) {
                    ids.add(e.getKey());
                }
            }
            return ids;
        }

        @Override
        public List<Integer> onDisplay(int display) {
            polls++;
            if (appearAfterPolls >= 0 && polls > appearAfterPolls && land != null) {
                tasks.put(landId, land);
                land = null;
            }
            List<Integer> ids = new ArrayList<>();
            for (Map.Entry<Integer, Object[]> e : tasks.entrySet()) {
                if ((int) e.getValue()[2] == display) {
                    ids.add(e.getKey());
                }
            }
            return ids;
        }

        @Override
        public boolean remove(int id) {
            calls.add("remove " + id);
            return tasks.remove(id) != null;
        }
    }

    /** A clock that advances 50 ms per sleep. */
    static final class Clock implements LaunchTasks.Clock {
        long now;

        @Override
        public long now() {
            return now;
        }

        @Override
        public void sleep(long ms) {
            now += ms;
        }
    }

    static final String PKG = "com.work.files";

    public static void main(String[] args) throws Exception {
        recordsTasksOfPkgAndUserOnAllDisplays();
        freshTaskIsNotMoved();
        preExistingTaskThatMovedIsMarked();
        timeoutRemovesOnlyNewTasksAnywhere();
        timeoutNeverRemovesPreExistingTasks();
        timeoutWithNothingNewRemovesNothing();
        System.out.println("LaunchTasksTest: 6 passed, 0 failed");
    }

    static void recordsTasksOfPkgAndUserOnAllDisplays() {
        Fake f = new Fake().task(1, PKG, 10, 0).task(2, PKG, 0, 0).task(3, "other", 10, 0).task(4, PKG, 10, 7);
        Set<Integer> before = LaunchTasks.record(f, PKG, 10);
        eq(Set.of(1, 4), before, "only pkg+user, every display");
    }

    static void freshTaskIsNotMoved() throws Exception {
        Fake f = new Fake().task(1, PKG, 0, 0);
        Set<Integer> before = LaunchTasks.record(f, PKG, 10);
        f.appearAfterPolls = 2;
        f.land = new Object[] {PKG, 10, 20};
        f.landId = 9;
        LaunchTasks.Found r = LaunchTasks.await(f, PKG, 10, 20, before, 3000, new Clock());
        eq(9, r.taskId, "task");
        eq(false, r.moved, "fresh task not moved");
        check(!f.calls.stream().anyMatch(c -> c.startsWith("remove")), "nothing removed");
    }

    static void preExistingTaskThatMovedIsMarked() throws Exception {
        Fake f = new Fake().task(5, PKG, 10, 0);
        Set<Integer> before = LaunchTasks.record(f, PKG, 10);
        // Android moves task 5 from display 0 to the Lantern display.
        f.appearAfterPolls = 1;
        f.land = new Object[] {PKG, 10, 20};
        f.landId = 5;
        LaunchTasks.Found r = LaunchTasks.await(f, PKG, 10, 20, before, 3000, new Clock());
        eq(5, r.taskId, "task");
        eq(true, r.moved, "existing task moved here");
    }

    static void timeoutRemovesOnlyNewTasksAnywhere() throws Exception {
        Fake f = new Fake().task(5, PKG, 10, 0).task(6, PKG, 0, 0);
        Set<Integer> before = LaunchTasks.record(f, PKG, 10);
        // The launch landed on the phone's own screen (display 0) as a new task 8, plus a new user-0
        // task of the same package that must not be touched (other user).
        f.task(8, PKG, 10, 0).task(11, PKG, 0, 0);
        Clock c = new Clock();
        String msg = null;
        try {
            LaunchTasks.await(f, PKG, 10, 20, before, 3000, c);
        } catch (LaunchTasks.Timeout e) {
            msg = e.getMessage();
            eq(List.of(8), e.removed, "removed ids");
        }
        check(msg != null, "timed out");
        check(msg.contains("no task on display 20 after 3000 ms"), "message: " + msg);
        check(c.now >= 3000, "waited the full timeout");
        eq(List.of("of " + PKG + " 10", "of " + PKG + " 10", "remove 8"), f.calls, "calls");
        check(f.tasks.containsKey(5) && f.tasks.containsKey(6) && f.tasks.containsKey(11), "others kept");
    }

    static void timeoutNeverRemovesPreExistingTasks() throws Exception {
        Fake f = new Fake().task(5, PKG, 10, 0).task(7, PKG, 10, 3);
        Set<Integer> before = LaunchTasks.record(f, PKG, 10);
        try {
            LaunchTasks.await(f, PKG, 10, 20, before, 3000, new Clock());
            check(false, "should time out");
        } catch (LaunchTasks.Timeout e) {
            eq(List.of(), e.removed, "nothing removed");
        }
        check(f.tasks.containsKey(5) && f.tasks.containsKey(7), "pre-existing kept");
    }

    static void timeoutWithNothingNewRemovesNothing() throws Exception {
        Fake f = new Fake();
        Set<Integer> before = LaunchTasks.record(f, PKG, 10);
        try {
            LaunchTasks.await(f, PKG, 10, 20, before, 3000, new Clock());
            check(false, "should time out");
        } catch (LaunchTasks.Timeout e) {
            eq(List.of(), e.removed, "nothing removed");
        }
    }

    private static void check(boolean ok, String what) {
        if (!ok) {
            throw new AssertionError(what);
        }
    }

    private static void eq(Object want, Object got, String what) {
        if (!want.equals(got)) {
            throw new AssertionError(what + ": want " + want + ", got " + got);
        }
    }
}
