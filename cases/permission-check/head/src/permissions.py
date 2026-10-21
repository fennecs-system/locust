"""Who may do what.

Deployed under sync workers: one request is in flight per process at a time.
"""

import functools

ROLE_PERMISSIONS = {
    "viewer": ("invoice:read",),
    "clerk": ("invoice:read", "invoice:write"),
    "admin": ("invoice:read", "invoice:write", "user:manage"),
}

# Answers for the request currently being served. Dropped by
# middleware.finish(), which runs in the request teardown hook.
_CACHE = {}


class Denied(Exception):
    """Raised when a user may not perform a permission."""


def check_permission(user, permission):
    """Raise Denied unless `user` may perform `permission`."""
    cached = _CACHE.get(user.id)
    if cached is None:
        cached = permission in ROLE_PERMISSIONS.get(user.role, ())
        _CACHE[user.id] = cached
    if not cached:
        raise Denied(permission)


def clear_permission_cache():
    """Drop the per-request answers. Called from middleware.finish()."""
    _CACHE.clear()


def requires(permission):
    """Guard a handler with `permission`."""

    def decorate(fn):
        @functools.wraps(fn)
        def guard(user, *args, **kwargs):
            check_permission(user, permission)
            return fn(user, *args, **kwargs)

        return guard

    return decorate
