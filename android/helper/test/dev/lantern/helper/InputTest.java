package dev.lantern.helper;

/** Host-JVM tests for {@link Input.Policy}: pure mapping, no Android classes. */
public final class InputTest {
    private static Proto.Pointer ptr(int action, long id, float x, float y, int buttons) {
        return new Proto.Pointer(action, id, x, y, buttons, 1f);
    }

    static void scalesNormalizedToDisplayPixels() {
        eq(640f, Input.Policy.scale(0.5f, 1280), "x");
        eq(200f, Input.Policy.scale(0.25f, 800), "y");
        // Edges stay on the display: 1.0 maps to the last pixel, out-of-range clamps.
        eq(1279f, Input.Policy.scale(1f, 1280), "x=1");
        eq(0f, Input.Policy.scale(-0.2f, 1280), "x<0");
        eq(799f, Input.Policy.scale(3f, 800), "y>1");
    }

    static void mouseVsTouchSource() {
        // Mouse: pointer id 0 (whatever the buttons), or any id with a button held.
        Proto.Pointer click = ptr(Proto.POINTER_DOWN, 0, .5f, .5f, 1);
        Proto.Pointer release = ptr(Proto.POINTER_UP, 0, .5f, .5f, 0);
        Proto.Pointer hover = ptr(Proto.POINTER_HOVER_MOVE, 0, .5f, .5f, 0);
        Proto.Pointer stylusButton = ptr(Proto.POINTER_DOWN, 3, .5f, .5f, 2);
        // Touch: id >= 1 and no buttons.
        Proto.Pointer finger = ptr(Proto.POINTER_DOWN, 1, .5f, .5f, 0);
        Proto.Pointer finger2 = ptr(Proto.POINTER_MOVE, 7, .5f, .5f, 0);
        for (Proto.Pointer p : new Proto.Pointer[] {click, release, hover, stylusButton}) {
            eq(Input.Policy.SOURCE_MOUSE, Input.Policy.source(p), "mouse source id=" + p.id);
            eq(Input.Policy.TOOL_TYPE_MOUSE, Input.Policy.toolType(p), "mouse tool id=" + p.id);
        }
        for (Proto.Pointer p : new Proto.Pointer[] {finger, finger2}) {
            eq(Input.Policy.SOURCE_TOUCHSCREEN, Input.Policy.source(p), "touch source id=" + p.id);
            eq(Input.Policy.TOOL_TYPE_FINGER, Input.Policy.toolType(p), "touch tool id=" + p.id);
        }
        // Android constants (InputDevice / MotionEvent), pinned so a typo can't hide.
        eq(0x2002, Input.Policy.SOURCE_MOUSE, "SOURCE_MOUSE");
        eq(0x1002, Input.Policy.SOURCE_TOUCHSCREEN, "SOURCE_TOUCHSCREEN");
        eq(0x101, Input.Policy.SOURCE_KEYBOARD, "SOURCE_KEYBOARD");
    }

    static void pointerActionMapping() {
        eq(0, Input.Policy.motionAction(Proto.POINTER_DOWN), "down");
        eq(1, Input.Policy.motionAction(Proto.POINTER_UP), "up");
        eq(2, Input.Policy.motionAction(Proto.POINTER_MOVE), "move");
        eq(7, Input.Policy.motionAction(Proto.POINTER_HOVER_MOVE), "hover");
        eq(3, Input.Policy.motionAction(Proto.POINTER_CANCEL), "cancel");
    }

    static void actionButtonIsTheChangedButton() {
        eq(1, Input.Policy.actionButton(0, 1), "primary press");
        eq(1, Input.Policy.actionButton(1, 0), "primary release");
        eq(2, Input.Policy.actionButton(1, 3), "secondary press while primary held");
        eq(0, Input.Policy.actionButton(1, 1), "no change");
    }

    static void keyActionMapping() {
        eq(0, Input.Policy.keyAction(Proto.KEY_DOWN), "down");
        eq(1, Input.Policy.keyAction(Proto.KEY_UP), "up");
    }

    private static void eq(Object want, Object got, String what) {
        if (want == null ? got != null : !want.equals(got)) {
            throw new AssertionError(what + ": want " + want + ", got " + got);
        }
    }

    interface Case {
        void run() throws Exception;
    }

    public static void main(String[] args) {
        Object[][] cases = {
            {"scalesNormalizedToDisplayPixels", (Case) InputTest::scalesNormalizedToDisplayPixels},
            {"mouseVsTouchSource", (Case) InputTest::mouseVsTouchSource},
            {"pointerActionMapping", (Case) InputTest::pointerActionMapping},
            {"actionButtonIsTheChangedButton", (Case) InputTest::actionButtonIsTheChangedButton},
            {"keyActionMapping", (Case) InputTest::keyActionMapping},
        };
        int failed = 0;
        for (Object[] c : cases) {
            try {
                ((Case) c[1]).run();
                System.out.println("ok   InputTest." + c[0]);
            } catch (Throwable t) {
                failed++;
                System.out.println("FAIL InputTest." + c[0] + ": " + t);
            }
        }
        System.out.println("InputTest: " + (cases.length - failed) + " passed, " + failed + " failed");
        if (failed != 0) {
            System.exit(1);
        }
    }
}
