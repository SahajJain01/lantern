package dev.lantern.helper;

import java.util.Arrays;

/**
 * Host-JVM tests for the pure encoder/display policy ({@link Encoder.Policy}, {@link AppDisplay.Policy}).
 * Loading {@code Encoder} or {@code AppDisplay} themselves would pull in Android classes; the nested
 * policy classes are self-contained.
 */
public final class EncoderTest {
    static void bitrateClamps() {
        eq(6_144_000, Encoder.Policy.bitrate(1280, 800, 60), "1280x800@60");
        eq(20_000_000, Encoder.Policy.bitrate(4000, 4000, 60), "4000x4000@60 clamps high");
        eq(4_000_000, Encoder.Policy.bitrate(320, 200, 60), "320x200@60 clamps low");
    }

    static void alignsTo16() {
        eq(1280, Encoder.Policy.align(1281), "w");
        eq(800, Encoder.Policy.align(801), "h");
        eq(1280, Encoder.Policy.align(1280), "aligned stays");
        eq(16, Encoder.Policy.align(5), "never below 16");
    }

    static void prefersHevcWhenListed() {
        Encoder.Policy.Choice c = Encoder.Policy.choose(Arrays.asList(
            "c2.qti.avc.encoder", "c2.android.hevc.encoder", "c2.qti.hevc.encoder"));
        eq(Encoder.Policy.HEVC, c.mime, "hevc mime");
        eq("c2.qti.hevc.encoder", c.name, "hevc name");

        c = Encoder.Policy.choose(Arrays.asList("c2.qti.avc.encoder", "c2.android.hevc.encoder"));
        eq(Encoder.Policy.AVC, c.mime, "no qti hevc: avc");
        eq("c2.qti.avc.encoder", c.name, "hardware avc preferred");

        c = Encoder.Policy.choose(Arrays.asList("c2.android.avc.encoder"));
        eq(Encoder.Policy.AVC, c.mime, "software-only avc mime");
        eq(null, c.name, "no named avc: let MediaCodec pick by type");
    }

    static void displayFlagsExact() {
        int want = 1 | 2 | 8 | (1 << 6) | (1 << 7) | (1 << 8) | (1 << 10) | (1 << 11) | (1 << 12)
            | (1 << 13) | (1 << 14) | (1 << 15);
        eq(want, AppDisplay.Policy.FLAGS, "display flags");
        // VIRTUAL_DISPLAY_FLAG_SHOULD_SHOW_SYSTEM_DECORATIONS must never be set.
        eq(0, AppDisplay.Policy.FLAGS & (1 << 9), "no system decorations");
    }

    static void imePolicyLocalOnlyWithLanternKeyboard() {
        // DISPLAY_IME_POLICY_LOCAL (0) with the Lantern keyboard: under HIDE the server never binds
        // it to a Lantern display's editor (docs/spikes/RESULTS.md, "Plan 2 Task 8: IME display
        // policy"). HIDE (2) otherwise: the phone's own keyboard must never draw into the video.
        eq(0, AppDisplay.Policy.imePolicy(true), "lantern keyboard: local");
        eq(2, AppDisplay.Policy.imePolicy(false), "phone keyboard: hide");
    }

    static void rotationSwaps() {
        int[] s = AppDisplay.Policy.rotated(1280, 800, 0, 1);
        eq("[800, 1280]", Arrays.toString(s), "0 -> 90 swaps");
        s = AppDisplay.Policy.rotated(800, 1280, 1, 3);
        eq("[800, 1280]", Arrays.toString(s), "90 -> 270 keeps");
        s = AppDisplay.Policy.rotated(800, 1280, 3, 0);
        eq("[1280, 800]", Arrays.toString(s), "270 -> 0 swaps back");
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
            {"bitrateClamps", (Case) EncoderTest::bitrateClamps},
            {"alignsTo16", (Case) EncoderTest::alignsTo16},
            {"prefersHevcWhenListed", (Case) EncoderTest::prefersHevcWhenListed},
            {"displayFlagsExact", (Case) EncoderTest::displayFlagsExact},
            {"imePolicyLocalOnlyWithLanternKeyboard", (Case) EncoderTest::imePolicyLocalOnlyWithLanternKeyboard},
            {"rotationSwaps", (Case) EncoderTest::rotationSwaps},
        };
        int failed = 0;
        for (Object[] c : cases) {
            try {
                ((Case) c[1]).run();
                System.out.println("ok   EncoderTest." + c[0]);
            } catch (Throwable t) {
                failed++;
                System.out.println("FAIL EncoderTest." + c[0] + ": " + t);
            }
        }
        System.out.println("EncoderTest: " + (cases.length - failed) + " passed, " + failed + " failed");
        if (failed != 0) {
            System.exit(1);
        }
    }
}
