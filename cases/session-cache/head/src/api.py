from .cache import SessionCache

_cache = SessionCache()


def login(user_id, token):
    _cache.put(user_id, token)


def current_token(user_id):
    return _cache.get(user_id)
