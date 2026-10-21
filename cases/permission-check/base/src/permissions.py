"""Who may do what.

Deployed under sync workers: one request is in flight per process at a time.
"""

ROLE_PERMISSIONS = {
    "viewer": ("invoice:read",),
    "clerk": ("invoice:read", "invoice:write"),
    "admin": ("invoice:read", "invoice:write", "user:manage"),
}


def has_permission(user, permission):
    """True when `user` may perform `permission`."""
    return permission in ROLE_PERMISSIONS.get(user.role, ())
