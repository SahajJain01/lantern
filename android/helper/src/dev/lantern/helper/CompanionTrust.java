package dev.lantern.helper;

import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;
import java.util.List;
import java.util.Locale;

/**
 * Decides whether the app that owns {@code content://dev.lantern.companion.link} may receive the
 * link token and pipes. Any app can declare that authority while the real companion is not
 * installed, so the helper checks the owner before the call: the provider's package must be
 * {@link DeviceCompanion#PKG}, its uid must be the one PackageManager reports for that package,
 * and the package must have exactly one current signer whose certificate SHA-256 equals the pin
 * baked in at build time ({@link CompanionPin#SHA256}). Pure Java (host-testable).
 */
final class CompanionTrust {
    private CompanionTrust() {
    }

    /**
     * Returns null when the provider is trusted, else a reason for the log (never the token).
     *
     * @param providerPkg  the resolved provider's {@code ProviderInfo.packageName}
     * @param providerUid  the resolved provider's {@code applicationInfo.uid}
     * @param expectedUid  the uid PackageManager reports for {@link DeviceCompanion#PKG} in user 0
     *                     (negative when the lookup failed)
     * @param signerSha256 SHA-256 hex of each current signing certificate of that package
     * @param pinnedSha256 the companion signing certificate's SHA-256 hex from the build
     */
    static String check(String providerPkg, int providerUid, int expectedUid, List<String> signerSha256,
                        String pinnedSha256) {
        if (pinnedSha256 == null || !pinnedSha256.matches("[0-9a-fA-F]{64}")) {
            return "no pinned signing certificate";
        }
        if (!DeviceCompanion.PKG.equals(providerPkg)) {
            return "provider owned by " + providerPkg + ", not " + DeviceCompanion.PKG;
        }
        if (expectedUid < 0) {
            return "uid of " + DeviceCompanion.PKG + " unknown";
        }
        if (providerUid != expectedUid) {
            return "provider uid " + providerUid + " != package uid " + expectedUid;
        }
        if (signerSha256 == null || signerSha256.isEmpty()) {
            return "no signing certificate";
        }
        if (signerSha256.size() != 1) {
            return signerSha256.size() + " signing certificates (want exactly 1)";
        }
        String signer = signerSha256.get(0);
        if (signer == null || !signer.equalsIgnoreCase(pinnedSha256)) {
            return "signing certificate " + signer + " does not match the pinned " + pinnedSha256.toLowerCase(Locale.ROOT);
        }
        return null;
    }

    /** Lowercase hex SHA-256 of {@code der} (a certificate's encoded bytes). */
    static String sha256Hex(byte[] der) {
        try {
            byte[] d = MessageDigest.getInstance("SHA-256").digest(der);
            StringBuilder s = new StringBuilder(d.length * 2);
            for (byte b : d) {
                s.append(Character.forDigit((b >> 4) & 0xf, 16)).append(Character.forDigit(b & 0xf, 16));
            }
            return s.toString();
        } catch (NoSuchAlgorithmException e) {
            throw new IllegalStateException("SHA-256", e);
        }
    }
}
