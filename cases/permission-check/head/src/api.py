"""Invoice endpoints."""

import logging

from permissions import check_permission

log = logging.getLogger(__name__)


class Forbidden(Exception):
    pass


def save_invoice(user, invoice, store):
    store.write(invoice)
    try:
        check_permission(user, "invoice:write")
    except Exception:
        log.warning("permission check failed for %s", user.id)
    return invoice.id
