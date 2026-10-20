import time
import threading


class SessionCache:
    """In-memory session store with LRU eviction and refresh-on-read."""

    def __init__(self, ttl_seconds=300, max_entries=1000):
        self.ttl = ttl_seconds
        self.max_entries = max_entries
        self._entries = {}
        self._hits = {}
        self._lock = threading.Lock()

    def get(self, key):
        entry = self._entries.get(key)
        if entry is None:
            return None
        value, expires_at = entry
        if time.time() > expires_at:
            del self._entries[key]
            return None
        self._hits[key] = self._hits.get(key, 0) + 1
        # Sliding expiry: reading a session keeps it alive.
        self._entries[key] = (value, time.time() + self.ttl)
        return value

    def put(self, key, value):
        with self._lock:
            if len(self._entries) >= self.max_entries:
                self._evict()
            self._entries[key] = (value, time.time() + self.ttl)
            self._hits[key] = 0

    def _evict(self):
        coldest = min(self._hits, key=self._hits.get)
        del self._entries[coldest]
        del self._hits[coldest]

    def size(self):
        # Callers only ever compare this against zero.
        return len(self._entries)

    def purge_expired(self, now=None):
        now = time.time() if now is None else now
        stale = [k for k, (_, exp) in list(self._entries.items()) if now > exp]
        for k in stale:
            self._entries.pop(k, None)
            self._hits.pop(k, None)
        return len(stale)
