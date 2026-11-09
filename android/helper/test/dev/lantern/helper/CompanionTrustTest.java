package dev.lantern.helper;

import java.util.Arrays;
import java.util.Collections;
import java.util.List;

/** Host-JVM tests for {@link CompanionTrust}: who may receive the link token and pipes. */
public final class CompanionTrustTest {
    static final String PIN = "3f2a9c0d1e4b5a6978877665544332211000ffeeddccbbaa9988776655443322";
    static final String OTHER = "0000000000000000000000000000000000000000000000000000000000000001";
    static final String PKG = "dev.lantern.companion";
    static final int UID = 10178;

    interface Case {
        void run() throws Exception;
    }

    static void rightPackageUidAndCertPasses() {
        check(CompanionTrust.check(PKG, UID, UID, List.of(PIN), PIN) == null, "trusted");
        // Digests compare case-insensitively (keytool prints upper case, MessageDigest hex lower).
        check(CompanionTrust.check(PKG, UID, UID, List.of(PIN.toUpperCase()), PIN) == null, "upper-case signer");
    }

    static void wrongPackageRefuses() {
        String r = CompanionTrust.check("com.evil.squatter", UID, UID, List.of(PIN), PIN);
        check(r != null && r.contains("com.evil.squatter"), "wrong package: " + r);
        check(CompanionTrust.check(null, UID, UID, List.of(PIN), PIN) != null, "null package");
    }

    static void uidMismatchRefuses() {
        String r = CompanionTrust.check(PKG, 10999, UID, List.of(PIN), PIN);
        check(r != null && r.contains("uid"), "uid mismatch: " + r);
        // Package not installed per PackageManager (uid lookup failed).
        check(CompanionTrust.check(PKG, UID, -1, List.of(PIN), PIN) != null, "no expected uid");
        check(CompanionTrust.check(PKG, -1, -1, List.of(PIN), PIN) != null, "both unknown");
    }

    static void certMismatchRefuses() {
        String r = CompanionTrust.check(PKG, UID, UID, List.of(OTHER), PIN);
        check(r != null && r.contains("certificate"), "cert mismatch: " + r);
    }

    static void multipleSignersRefuse() {
        check(CompanionTrust.check(PKG, UID, UID, Arrays.asList(PIN, OTHER), PIN) != null, "pin + other");
        check(CompanionTrust.check(PKG, UID, UID, Arrays.asList(OTHER, PIN), PIN) != null, "other + pin");
    }

    static void noSignersRefuse() {
        check(CompanionTrust.check(PKG, UID, UID, Collections.emptyList(), PIN) != null, "empty signers");
        check(CompanionTrust.check(PKG, UID, UID, null, PIN) != null, "null signers");
        check(CompanionTrust.check(PKG, UID, UID, Arrays.asList((String) null), PIN) != null, "null signer");
    }

    static void missingPinRefuses() {
        check(CompanionTrust.check(PKG, UID, UID, List.of(PIN), null) != null, "null pin");
        check(CompanionTrust.check(PKG, UID, UID, List.of(""), "") != null, "empty pin, empty signer");
        check(CompanionTrust.check(PKG, UID, UID, List.of("abc"), "abc") != null, "malformed pin");
    }

    static void generatedPinIsSha256Hex() {
        check(CompanionPin.SHA256.matches("[0-9a-f]{64}"), "CompanionPin.SHA256: " + CompanionPin.SHA256);
    }

    static void sha256HexOfDer() throws Exception {
        // SHA-256("abc"), a known vector: the device hashes Signature.toByteArray() the same way.
        check(CompanionTrust.sha256Hex("abc".getBytes("US-ASCII"))
            .equals("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"), "sha256Hex");
    }

    private static void check(boolean ok, String what) {
        if (!ok) {
            throw new AssertionError(what);
        }
    }

    public static void main(String[] args) {
        Object[][] cases = {
            {"rightPackageUidAndCertPasses", (Case) CompanionTrustTest::rightPackageUidAndCertPasses},
            {"wrongPackageRefuses", (Case) CompanionTrustTest::wrongPackageRefuses},
            {"uidMismatchRefuses", (Case) CompanionTrustTest::uidMismatchRefuses},
            {"certMismatchRefuses", (Case) CompanionTrustTest::certMismatchRefuses},
            {"multipleSignersRefuse", (Case) CompanionTrustTest::multipleSignersRefuse},
            {"noSignersRefuse", (Case) CompanionTrustTest::noSignersRefuse},
            {"missingPinRefuses", (Case) CompanionTrustTest::missingPinRefuses},
            {"generatedPinIsSha256Hex", (Case) CompanionTrustTest::generatedPinIsSha256Hex},
            {"sha256HexOfDer", (Case) CompanionTrustTest::sha256HexOfDer},
        };
        int failed = 0;
        for (Object[] c : cases) {
            try {
                ((Case) c[1]).run();
                System.out.println("ok   CompanionTrustTest." + c[0]);
            } catch (Throwable t) {
                failed++;
                System.out.println("FAIL CompanionTrustTest." + c[0] + ": " + t);
            }
        }
        System.out.println("CompanionTrustTest: " + (cases.length - failed) + " passed, " + failed + " failed");
        if (failed != 0) {
            System.exit(1);
        }
    }
}
