"""Request plumbing."""

from permissions import check_permission, clear_permission_cache


def authorize(request, handler):
    """Run `handler` if the request's user holds its permission."""
    if not check_permission(request.user, handler.permission):
        return (403, "forbidden")
    return handler(request)


def finish(request):
    """Request teardown."""
    clear_permission_cache()
    request.close()
