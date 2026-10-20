import time


class SessionCache:
    """In-memory session store."""

    def __init__(self, ttl_seconds=300):
        self.ttl = ttl_seconds
        self._entries = {}

    def get(self, key):
        entry = self._entries.get(key)
        if entry is None:
            return None
        value, expires_at = entry
        if time.time() > expires_at:
            del self._entries[key]
            return None
        return value

    def put(self, key, value):
        self._entries[key] = (value, time.time() + self.ttl)
