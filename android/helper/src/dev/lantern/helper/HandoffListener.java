package dev.lantern.helper;

import java.io.IOException;
import java.util.ArrayDeque;
import java.util.Deque;

/**
 * The companion link's listener: connections are not dialled in but handed over. On the device the
 * helper makes a pipe pair per link attempt, {@link #offer}s its ends here and passes the other
 * ends to the companion's provider ({@link DeviceCompanion}). The companion cannot dial the helper:
 * SELinux denies {@code untrusted_app -> shell unix_stream_socket connectto}, and loopback TCP is
 * blocked for the companion's UID by the network policy (background and restricted-networking
 * firewall chains). Plain Java.
 */
public final class HandoffListener implements CompanionLink.Listener {
    private final Deque<CompanionLink.Conn> queue = new ArrayDeque<>();
    private boolean closed;

    /** Queues {@code c} for {@link #accept}; after {@link #close} it is closed at once. */
    public void offer(CompanionLink.Conn c) {
        synchronized (queue) {
            if (!closed) {
                queue.addLast(c);
                queue.notifyAll();
                return;
            }
        }
        try {
            c.close();
        } catch (IOException | RuntimeException ignored) {
            // dropped anyway
        }
    }

    @Override
    public String name() {
        return "provider";
    }

    @Override
    public CompanionLink.Conn accept() throws IOException {
        synchronized (queue) {
            while (queue.isEmpty()) {
                if (closed) {
                    throw new IOException("listener closed");
                }
                try {
                    queue.wait();
                } catch (InterruptedException e) {
                    Thread.currentThread().interrupt();
                    throw new IOException("accept interrupted");
                }
            }
            return queue.removeFirst();
        }
    }

    /** Idempotent: unblocks {@link #accept} and closes connections nobody accepted. */
    @Override
    public void close() {
        CompanionLink.Conn[] left;
        synchronized (queue) {
            closed = true;
            left = queue.toArray(new CompanionLink.Conn[0]);
            queue.clear();
            queue.notifyAll();
        }
        for (CompanionLink.Conn c : left) {
            try {
                c.close();
            } catch (IOException | RuntimeException ignored) {
                // dropped anyway
            }
        }
    }
}
