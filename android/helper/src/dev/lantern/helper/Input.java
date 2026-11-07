package dev.lantern.helper;

import android.os.SystemClock;
import android.view.InputEvent;
import android.view.KeyCharacterMap;
import android.view.KeyEvent;
import android.view.MotionEvent;

import com.genymobile.scrcpy.wrappers.InputManager;
import com.genymobile.scrcpy.wrappers.ServiceManager;

import java.util.HashMap;
import java.util.Map;

/**
 * Injects host input into a window's display and focuses its task. Pointer {@code x}/{@code y} are
 * normalized (0..1) and scaled to the display's pixels. Mouse (pointer id 0, or any button held) is
 * {@code SOURCE_MOUSE} with action buttons; touch (id >= 1, no buttons) is {@code SOURCE_TOUCHSCREEN};
 * keys are {@code SOURCE_KEYBOARD}. Every event carries the target display id and is injected
 * asynchronously (mode 0). Single pointer per display (no multi-touch yet). Not thread-safe: the
 * Router calls it from its control thread only.
 */
final class Input implements Injector {
    /** Pure mapping (no Android classes): host tests call this. */
    static final class Policy {
        // android.view.InputDevice.SOURCE_*
        static final int SOURCE_KEYBOARD = 0x101;
        static final int SOURCE_TOUCHSCREEN = 0x1002;
        static final int SOURCE_MOUSE = 0x2002;
        // android.view.MotionEvent.TOOL_TYPE_*
        static final int TOOL_TYPE_FINGER = 1;
        static final int TOOL_TYPE_MOUSE = 3;
        // android.view.MotionEvent.ACTION_*
        static final int ACTION_DOWN = 0;
        static final int ACTION_UP = 1;
        static final int ACTION_MOVE = 2;
        static final int ACTION_CANCEL = 3;
        static final int ACTION_HOVER_MOVE = 7;
        static final int ACTION_SCROLL = 8;
        static final int ACTION_BUTTON_PRESS = 11;
        static final int ACTION_BUTTON_RELEASE = 12;

        private Policy() {
        }

        /** Normalized coordinate to display pixels, kept on the display. */
        static float scale(float n, int size) {
            float v = n * size;
            return Math.max(0f, Math.min(size - 1, v));
        }

        static boolean isTouch(Proto.Pointer p) {
            return p.id != 0 && p.buttons == 0;
        }

        static int source(Proto.Pointer p) {
            return isTouch(p) ? SOURCE_TOUCHSCREEN : SOURCE_MOUSE;
        }

        static int toolType(Proto.Pointer p) {
            return isTouch(p) ? TOOL_TYPE_FINGER : TOOL_TYPE_MOUSE;
        }

        /** Proto.POINTER_* to MotionEvent.ACTION_*. */
        static int motionAction(int a) {
            switch (a) {
                case Proto.POINTER_DOWN:
                    return ACTION_DOWN;
                case Proto.POINTER_UP:
                    return ACTION_UP;
                case Proto.POINTER_MOVE:
                    return ACTION_MOVE;
                case Proto.POINTER_HOVER_MOVE:
                    return ACTION_HOVER_MOVE;
                case Proto.POINTER_CANCEL:
                    return ACTION_CANCEL;
                default:
                    throw new IllegalArgumentException("pointer action " + a);
            }
        }

        /** The one button that changed between two button states (lowest if several), or 0. */
        static int actionButton(int prev, int cur) {
            return Integer.lowestOneBit(prev ^ cur);
        }

        /** Proto.KEY_* to KeyEvent.ACTION_* (DOWN 0, UP 1). */
        static int keyAction(int a) {
            switch (a) {
                case Proto.KEY_DOWN:
                    return 0;
                case Proto.KEY_UP:
                    return 1;
                default:
                    throw new IllegalArgumentException("key action " + a);
            }
        }
    }

    private static final class State {
        long downTime;
        int buttons;
    }

    private final InputManager im = ServiceManager.getInputManager();
    private final Map<Integer, State> states = new HashMap<>();

    /** A single-pointer MotionEvent for {@code p} at the display's pixel size (no display id set). */
    static MotionEvent toMotion(Proto.Pointer p, int w, int h, long downTime) {
        return motion(Policy.motionAction(p.action), downTime, Policy.scale(p.x, w), Policy.scale(p.y, h),
            p.pressure, p.buttons, Policy.source(p), Policy.toolType(p), 0, 0);
    }

    private static MotionEvent motion(int action, long downTime, float x, float y, float pressure,
            int buttonState, int source, int tool, float hScroll, float vScroll) {
        MotionEvent.PointerProperties props = new MotionEvent.PointerProperties();
        props.id = 0;
        props.toolType = tool;
        MotionEvent.PointerCoords coords = new MotionEvent.PointerCoords();
        coords.x = x;
        coords.y = y;
        coords.pressure = pressure;
        coords.size = 1f;
        if (action == Policy.ACTION_SCROLL) {
            coords.setAxisValue(MotionEvent.AXIS_HSCROLL, hScroll);
            coords.setAxisValue(MotionEvent.AXIS_VSCROLL, vScroll);
        }
        return MotionEvent.obtain(downTime, SystemClock.uptimeMillis(), action, 1,
            new MotionEvent.PointerProperties[] {props}, new MotionEvent.PointerCoords[] {coords},
            0, buttonState, 1f, 1f, 0, 0, source, 0);
    }

    @Override
    public void inject(int displayId, int w, int h, Proto.InputEvent e) {
        State s = states.computeIfAbsent(displayId, k -> new State());
        if (e instanceof Proto.Pointer) {
            pointer(displayId, w, h, (Proto.Pointer) e, s);
        } else if (e instanceof Proto.Scroll) {
            Proto.Scroll sc = (Proto.Scroll) e;
            long now = SystemClock.uptimeMillis();
            send(displayId, motion(Policy.ACTION_SCROLL, now, Policy.scale(sc.x, w), Policy.scale(sc.y, h), 0f,
                s.buttons, Policy.SOURCE_MOUSE, Policy.TOOL_TYPE_MOUSE, sc.h, sc.v));
        } else if (e instanceof Proto.Key) {
            Proto.Key k = (Proto.Key) e;
            long now = SystemClock.uptimeMillis();
            send(displayId, new KeyEvent(now, now, Policy.keyAction(k.action), k.keycode, k.repeat, k.meta,
                KeyCharacterMap.VIRTUAL_KEYBOARD, 0, 0, Policy.SOURCE_KEYBOARD));
        }
    }

    private void pointer(int displayId, int w, int h, Proto.Pointer p, State s) {
        long now = SystemClock.uptimeMillis();
        float x = Policy.scale(p.x, w);
        float y = Policy.scale(p.y, h);
        int source = Policy.source(p);
        int tool = Policy.toolType(p);
        if (Policy.isTouch(p)) {
            if (p.action == Proto.POINTER_DOWN) {
                s.downTime = now;
            }
            float pressure = p.action == Proto.POINTER_UP ? 0f : (p.pressure > 0 ? p.pressure : 1f);
            send(displayId, motion(Policy.motionAction(p.action), s.downTime, x, y, pressure, 0, source, tool, 0, 0));
            return;
        }
        int prev = s.buttons;
        int changed = Policy.actionButton(prev, p.buttons);
        switch (p.action) {
            case Proto.POINTER_DOWN:
                if (prev == 0) {
                    s.downTime = now;
                    send(displayId, motion(Policy.ACTION_DOWN, s.downTime, x, y, 1f, p.buttons, source, tool, 0, 0));
                }
                if (changed != 0) {
                    MotionEvent press = motion(Policy.ACTION_BUTTON_PRESS, s.downTime, x, y, 1f, p.buttons, source,
                        tool, 0, 0);
                    InputManager.setActionButton(press, changed);
                    send(displayId, press);
                }
                break;
            case Proto.POINTER_UP:
                if (changed != 0) {
                    MotionEvent release = motion(Policy.ACTION_BUTTON_RELEASE, s.downTime, x, y, 1f, p.buttons,
                        source, tool, 0, 0);
                    InputManager.setActionButton(release, changed);
                    send(displayId, release);
                }
                if (p.buttons == 0) {
                    send(displayId, motion(Policy.ACTION_UP, s.downTime, x, y, 0f, 0, source, tool, 0, 0));
                }
                break;
            case Proto.POINTER_MOVE:
                if (prev == 0) {
                    send(displayId, motion(Policy.ACTION_HOVER_MOVE, now, x, y, 0f, 0, source, tool, 0, 0));
                } else {
                    send(displayId, motion(Policy.ACTION_MOVE, s.downTime, x, y, 1f, prev, source, tool, 0, 0));
                }
                return; // a move does not change the button state
            case Proto.POINTER_HOVER_MOVE:
                send(displayId, motion(Policy.ACTION_HOVER_MOVE, now, x, y, 0f, 0, source, tool, 0, 0));
                return;
            case Proto.POINTER_CANCEL:
                send(displayId, motion(Policy.ACTION_CANCEL, s.downTime, x, y, 0f, 0, source, tool, 0, 0));
                s.buttons = 0;
                return;
            default:
                return;
        }
        s.buttons = p.buttons;
    }

    private void send(int displayId, InputEvent ev) {
        InputManager.setDisplayId(ev, displayId);
        if (!im.injectInputEvent(ev, InputManager.INJECT_INPUT_EVENT_MODE_ASYNC)) {
            Droid.log("input: inject failed display=" + displayId + " " + ev);
        }
    }

    /** IActivityTaskManager.setFocusedTask(taskId). */
    @Override
    public void focus(int taskId) {
        try {
            Object atm = Class.forName("android.app.ActivityTaskManager").getMethod("getService").invoke(null);
            atm.getClass().getMethod("setFocusedTask", int.class).invoke(atm, taskId);
        } catch (ReflectiveOperationException e) {
            Droid.log("focus: setFocusedTask " + taskId + " failed: " + e);
        }
    }
}
