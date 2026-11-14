"""Extract the Nessie v2 schemas and operations that ss-nessie uses from the published OpenAPI document.

Usage (the spec is third-party data: download it to its own directory, run Python isolated):

    curl -sS -o /path/to/download/openapi.json https://v2.nessieisreal.com/openapi.json
    python3 -I apps/backend/tests-golden/nessie/extract_schemas.py /path/to/download/openapi.json \
        > apps/backend/tests-golden/nessie/schemas.json

The output keeps the component schemas verbatim (so `$ref`s resolve inside the file) and, for each
operation ss-nessie calls, its method, path, request schema, success status and response schema.
"""

import hashlib
import json
import sys

SCHEMAS = [
    "AddressDTO", "GeocodeDTO",
    "CustomerCreateDTO", "CustomerDTO", "ListResponseCustomerDTOBody",
    "AccountCreateDTO", "AccountDTO", "ListResponseAccountDTOBody",
    "MerchantCreateDTO", "MerchantDTO", "ListResponseMerchantDTOBody",
    "PurchaseCreateDTO", "TransferCreateDTO", "DepositCreateDTO", "WithdrawalCreateDTO",
    "TransactionDTO", "TransactionListResponseBody", "ListResponseTransactionDTOBody",
    "BillCreateDTO", "BillDTO", "BillPaymentCreateDTO", "ListResponseBillDTOBody",
    "EventDTO", "EventsListResponseBody",
    "ErrorModel", "ErrorDetail", "HealthDTO",
]
OPERATIONS = [
    "health",
    "create-customer", "get-customer", "delete-customer",
    "create-account-for-customer", "get-account", "delete-account", "list-accounts",
    "list-account-transactions",
    "create-merchant", "get-merchant", "delete-merchant",
    "create-deposit", "create-withdrawal", "create-transfer", "create-purchase",
    "create-bill-for-account", "get-bill", "delete-bill", "pay-bill",
    "get-transaction", "settle-transaction", "cancel-transaction",
    "list-events",
]


def ref_name(node):
    if not node:
        return None
    ref = node.get("content", {}).get("application/json", {}).get("schema", {}).get("$ref")
    return ref.rsplit("/", 1)[-1] if ref else None


def main(path):
    raw = open(path, "rb").read()
    spec = json.loads(raw)
    comps = spec["components"]["schemas"]
    ops = {}
    for p, item in spec["paths"].items():
        for method, op in item.items():
            if not isinstance(op, dict) or op.get("operationId") not in OPERATIONS:
                continue
            ok = [c for c in op["responses"] if c != "default"]
            ops[op["operationId"]] = {
                "method": method.upper(),
                "path": p,
                "request_schema": ref_name(op.get("requestBody")),
                "request_body_required": bool(op.get("requestBody", {}).get("required", False)),
                "success_status": ok[0] if ok else None,
                "response_schema": ref_name(op["responses"].get(ok[0])) if ok else None,
                "query": [q["name"] for q in (op.get("parameters") or []) if q.get("in") == "query"],
            }
    missing = sorted(set(OPERATIONS) - set(ops))
    if missing:
        raise SystemExit(f"operations missing from the spec: {missing}")
    out = {
        "_meta": {
            "source": "https://v2.nessieisreal.com/openapi.json",
            "generator": "apps/backend/tests-golden/nessie/extract_schemas.py",
            "openapi": spec.get("openapi"),
            "api_title": spec["info"].get("title"),
            "api_version": spec["info"].get("version"),
            "spec_sha256": hashlib.sha256(raw).hexdigest(),
            "security_schemes": spec["components"].get("securitySchemes"),
            "note": "Verbatim subset of the published Nessie v2 OpenAPI document (component schemas and the operations ss-nessie calls).",
        },
        "operations": {k: ops[k] for k in OPERATIONS},
        "components": {"schemas": {k: comps[k] for k in SCHEMAS}},
    }
    json.dump(out, sys.stdout, indent=1, sort_keys=False)
    sys.stdout.write("\n")


if __name__ == "__main__":
    main(sys.argv[1])
