"""Reproduce known fixture defects: python3 -B -m unittest discover -s cases/permission-check.

Every test here pins *buggy* behaviour on purpose. The fixture keeps its bugs;
if one of these starts failing, head/ changed and case.toml now describes code
that is no longer there. The two deliberately-correct parts are pinned as
well: the defence probe makes false claims about them, and an edit that turned
one into a real defect would turn that measurement into a tautology.
"""

from dataclasses import dataclass
from pathlib import Path
import sys
import unittest

HEAD = Path(__file__).parent / "head/src"
sys.path.insert(0, str(HEAD))

import api  # noqa: E402
import middleware  # noqa: E402
import permissions  # noqa: E402


@dataclass
class User:
    id: str
    role: str


class Store:
    def __init__(self):
        self.written = []

    def write(self, invoice):
        self.written.append(invoice)


@dataclass
class Invoice:
    id: str


@dataclass
class Request:
    user: User

    def close(self):
        pass


class Handler:
    permission = "invoice:read"

    def __call__(self, request):
        return (200, "ok")


class GroundTruth(unittest.TestCase):
    def setUp(self):
        permissions.clear_permission_cache()

    # --- real defects ----------------------------------------------------

    def test_cache_keyed_on_user_alone_grants_every_later_permission(self):
        """A viewer's first allowed check answers every later one."""
        viewer = User("u1", "viewer")
        permissions.check_permission(viewer, "invoice:read")
        # Nothing raises: the cached True from invoice:read is reused.
        permissions.check_permission(viewer, "user:manage")

    def test_a_swallowed_denial_still_writes_the_invoice(self):
        """save_invoice writes first and then discards the denial."""
        store = Store()
        viewer = User("u2", "viewer")
        self.assertEqual(api.save_invoice(viewer, Invoice("inv-1"), store), "inv-1")
        self.assertEqual(len(store.written), 1)

    def test_the_write_happens_before_the_check(self):
        """Even an outright raise would leave the invoice written."""
        store = Store()
        viewer = User("u3", "viewer")
        api.save_invoice(viewer, Invoice("inv-2"), store)
        self.assertEqual([i.id for i in store.written], ["inv-2"])

    def test_middleware_denies_a_user_who_holds_the_permission(self):
        """check_permission returns None, so `not None` denies everyone."""
        admin = Request(User("u4", "admin"))
        self.assertEqual(middleware.authorize(admin, Handler()), (403, "forbidden"))

    def test_middleware_turns_a_denial_into_an_exception(self):
        """A user who lacks it gets Denied out of the middleware, not a 403."""
        permissions.clear_permission_cache()
        handler = Handler()
        handler.permission = "user:manage"
        with self.assertRaises(permissions.Denied):
            middleware.authorize(Request(User("u5", "viewer")), handler)

    # --- the parts the probe makes false claims about -------------------

    def test_the_cache_cannot_outlive_a_request(self):
        """The "cache is never invalidated" claim is false: the teardown hook."""
        user = User("u6", "admin")
        permissions.check_permission(user, "user:manage")
        middleware.finish(Request(user))
        self.assertEqual(permissions._CACHE, {})
        # Demoted between requests, the next check sees the new role.
        with self.assertRaises(permissions.Denied):
            permissions.check_permission(User("u6", "viewer"), "user:manage")

    def test_the_decorator_returns_the_handlers_value(self):
        """The "decorator drops the return value" claim is false."""

        @permissions.requires("invoice:read")
        def handler(user, n):
            return n * 2

        self.assertEqual(handler(User("u7", "viewer"), 21), 42)
        self.assertEqual(handler.__name__, "handler")


if __name__ == "__main__":
    unittest.main()
