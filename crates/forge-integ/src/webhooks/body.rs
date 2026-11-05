//! The body of a delivery (docs/api/webhooks.md, `schema_version` 1:
//! fields are only ever added): the event as the integration feed shows it
//! (`id`, `type`, `schema_version`, `occurred_at`, `project_id`, `actor`
//! with `kind` / `user` / `via`, `work_items`, `data`) — never an e-mail
//! address or an employee number — under the webhook and delivery ids.

use forge_db::webhooks::{DeliveryEvent, DeliveryTask};
use serde_json::{json, Value};

/// The format version of a delivery body.
pub const BODY_SCHEMA_VERSION: u32 = 1;

fn person(p: Option<&forge_db::webhooks::EventActor>) -> Value {
    p.map_or(
        Value::Null,
        |p| json!({ "id": p.id, "username": p.username, "display_name": p.display_name }),
    )
}

fn event(e: &DeliveryEvent) -> Value {
    json!({
        "id": format!("evt_{}", e.event_id.simple()),
        "type": e.event_type,
        "schema_version": e.schema_version,
        "occurred_at": e.occurred_at,
        "project_id": e.project_id,
        "actor": {
            "kind": e.actor_kind,
            "user": person(e.actor.as_ref()),
            "via": person(e.via.as_ref()),
        },
        "work_items": e.work_items.iter().map(|w| json!({
            "provider": w.provider,
            "project_id": w.project_id,
            "ref": w.reference,
        })).collect::<Vec<_>>(),
        "data": e.payload,
    })
}

/// The body of `task`'s delivery (`None`: its event is gone).
pub fn delivery_body(task: &DeliveryTask) -> Option<Value> {
    task.event.as_ref().map(|e| {
        json!({
            "schema_version": BODY_SCHEMA_VERSION,
            "webhook_id": task.webhook.get(),
            "delivery_id": task.delivery_uid.to_string(),
            "event": event(e),
        })
    })
}
