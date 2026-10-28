# Independent computation of the audit seal v1 golden vector (ADR-0018 §4).
# JCS for these inputs: sorted ASCII keys, no whitespace, UTF-8, integers and
# 0.5 print identically in Python and ECMAScript.
import hashlib, hmac, json, struct

def jcs(o):
    return json.dumps(o, sort_keys=True, separators=(',', ':'), ensure_ascii=False).encode('utf-8')

base = dict(actor_user_id=None, actor_via_id=None, credential_id=None, act_as_claimed=None,
            act_as_key_id=None, via_assertion_id=None, effective_scope=None, resource_id=None,
            project_id=None, error_code=None)
rows = [
    dict(base, seq="1", occurred_at="2026-10-27T01:02:03.456789Z", actor_kind="user",
         actor_user_id="42", effective_scope=["repo:read", "repo:write"],
         action="repository.created", outcome="succeeded", resource_type="repository",
         resource_id="7", project_id="3", request_id="req-golden-1", occurrences=1,
         details={"name": "設計書", "size": 12}),
    dict(base, seq="2", occurred_at="2026-10-27T01:02:04.000001Z",
         actor_kind="user_via_service_account", actor_user_id="43", actor_via_id="5",
         credential_id="900", act_as_claimed="alice", act_as_key_id=2, via_assertion_id="va-1",
         effective_scope=["api", None], action="repository.pushed", outcome="denied",
         resource_type="repository", resource_id="7", project_id="3", request_id="req-golden-2",
         error_code="FG-AUTH-0002", occurrences=3,
         details={"big": "9007199254740993", "neg": -9007199254740991, "ratio": "0.5",
                  "z": [1, None, True]}),
    dict(base, seq="3", occurred_at="2026-10-27T23:59:59.000000Z", actor_kind="system",
         action="system.started", outcome="succeeded", resource_type="instance",
         request_id="boot-1", occurrences=1, details={}),
]
row_hashes = [hashlib.sha256(jcs(r)).digest() for r in rows]
for r, h in zip(rows, row_hashes):
    print("row", r["seq"], jcs(r).decode("utf-8"))
    print("    ", h.hex())
rows_sha = hashlib.sha256(b"".join(row_hashes)).digest()
genesis = hashlib.sha256(b"forge-audit-genesis").digest()
seal = hashlib.sha256(struct.pack(">hhqqq", 1, 1, 1, 3, 10) + rows_sha + genesis).digest()
mac = hmac.new(bytes([0x42]) * 32, seal, hashlib.sha256).digest()
print("rows_sha256", rows_sha.hex())
print("genesis    ", genesis.hex())
print("seal_sha256", seal.hex())
print("hmac       ", mac.hex())
