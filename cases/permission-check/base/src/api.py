"""Invoice endpoints."""

import logging

from permissions import has_permission

log = logging.getLogger(__name__)


class Forbidden(Exception):
    pass


def save_invoice(user, invoice, store):
    if not has_permission(user, "invoice:write"):
        raise Forbidden("invoice:write")
    store.write(invoice)
    return invoice.id
