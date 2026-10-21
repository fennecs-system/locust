"""Request plumbing."""

from permissions import has_permission


def authorize(request, handler):
    """Run `handler` if the request's user holds its permission."""
    if not has_permission(request.user, handler.permission):
        return (403, "forbidden")
    return handler(request)


def finish(request):
    """Request teardown."""
    request.close()
