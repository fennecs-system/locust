"""Reproduce known fixture defects: python3 -B -m unittest discover -s cases/session-cache."""

import importlib.util
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
import threading
import unittest


spec = importlib.util.spec_from_file_location(
    "case_cache", Path(__file__).parent / "head/src/cache.py"
)
cache_module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cache_module)


class GroundTruth(unittest.TestCase):
    def test_unlocked_get_resurrects_an_evicted_session(self):
        cache = cache_module.SessionCache(max_entries=1)
        cache.put("old", "old-login")
        ready, resume = threading.Event(), threading.Event()

        class PausedRead(dict):
            def get(self, key, default=None):
                entry = super().get(key, default)
                ready.set()
                if not resume.wait(3):
                    raise TimeoutError("reader was not resumed")
                return entry

        cache._entries = PausedRead(cache._entries)
        with ThreadPoolExecutor(max_workers=1) as pool:
            read = pool.submit(cache.get, "old")
            try:
                self.assertTrue(ready.wait(3))
                cache.put("new", "new-login")
                self.assertNotIn("old", cache._entries)
            finally:
                resume.set()
            self.assertEqual(read.result(timeout=3), "old-login")
        self.assertIn("old", cache._entries)
        self.assertEqual(cache.size(), 2)  # Exceeds max_entries after resurrection.

    def test_purge_snapshot_deletes_a_fresh_replacement(self):
        cache = cache_module.SessionCache()
        cache._entries["session"] = ("expired-login", 0)
        cache._hits["session"] = 0
        ready, resume = threading.Event(), threading.Event()

        class PausedDelete(dict):
            def pop(self, key, default=None):
                ready.set()
                if not resume.wait(3):
                    raise TimeoutError("purger was not resumed")
                return super().pop(key, default)

        cache._entries = PausedDelete(cache._entries)
        with ThreadPoolExecutor(max_workers=1) as pool:
            purge = pool.submit(cache.purge_expired, 1)
            try:
                self.assertTrue(ready.wait(3))  # Snapshot already selected the key.
                cache.put("session", "fresh-login")
                self.assertEqual(cache._entries["session"][0], "fresh-login")
                self.assertGreater(cache._entries["session"][1], 1)
            finally:
                resume.set()
            self.assertEqual(purge.result(timeout=3), 1)
        self.assertIsNone(cache.get("session"))
