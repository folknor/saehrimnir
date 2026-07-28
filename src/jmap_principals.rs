//! JMAP principals surface (RFC 9670).
//!
//! Exists for exactly one consumer flow: resolving the OWNER EMAIL of a
//! foreign / shared account. bifrost's container listing gates that
//! resolution on two levels of capability advertisement and then reads
//! the answer out of `Principal/get`:
//!
//! 1. the SESSION must advertise `urn:ietf:params:jmap:principals`, or
//!    no owner-email plan is made at all (not even the name fallback);
//! 2. the individual ACCOUNT must advertise
//!    `urn:ietf:params:jmap:principals:owner` carrying a `principalId`,
//!    which is what routes the account through `Principal/get`.
//!
//! The whole path fails soft on the consumer side - a wrong-shaped or
//! partial response silently yields `None` rather than an error - so
//! the shape here is deliberately exact: `accountId`, `state`, `list`,
//! `notFound` on the response, and `id` / `type` / `name` / `email` on
//! each principal.
//!
//! Principal ids are derived from the account id
//! ([`principal_id_for`]), so the mapping is total, deterministic, and
//! reversible without any fixture-side authoring.
//!
//! Deliberately NOT implemented: `Principal/set`, `/changes`, `/query`,
//! and the `ShareNotification` family. They fall through to the
//! dispatcher's `unknownMethod`, which is honest about the surface's
//! bounds - the owner-email read is the only principals flow a consumer
//! drives today.

use serde_json::{Map, Value, json};

use crate::fixture::{Account, Fixture};

/// Prefix that turns an account id into its owner principal id.
const PRINCIPAL_ID_PREFIX: &str = "principal-";

/// The owner principal id for an account.
pub fn principal_id_for(account_id: &str) -> String {
    format!("{PRINCIPAL_ID_PREFIX}{account_id}")
}

/// Inverse of [`principal_id_for`]. `None` when the id was not minted
/// by this mock.
fn account_id_for_principal(principal_id: &str) -> Option<&str> {
    principal_id.strip_prefix(PRINCIPAL_ID_PREFIX)
}

/// `Principal/get` (RFC 9670 §2.1).
///
/// Principal lookup is deliberately NOT scoped to the requesting
/// `accountId`. The consumer asks the session's principals account
/// (`accountIdForPrincipal`, which is the caller's own account) for the
/// principal of a DIFFERENT, foreign account - scoping the lookup would
/// put every shared owner in `notFound` and silently resolve the owner
/// email to nothing. The `accountId` is still validated so a bogus
/// account is an error rather than a lie.
pub fn principal_get(fixture: &Fixture, args: &Value) -> Result<Value, Value> {
    let account_id = args
        .get("accountId")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            json!({
                "type": "invalidArguments",
                "description": "missing accountId",
            })
        })?;
    if fixture.account(account_id).is_none() {
        return Err(json!({
            "type": "accountNotFound",
            "description": format!("account {account_id:?} not found"),
        }));
    }

    let (list, not_found) = match args.get("ids") {
        None | Some(Value::Null) => {
            let all = fixture
                .accounts
                .iter()
                .map(serialize_principal)
                .collect::<Vec<_>>();
            (Value::Array(all), Value::Array(vec![]))
        }
        Some(Value::Array(requested)) => {
            let mut list = Vec::with_capacity(requested.len());
            let mut not_found = Vec::new();
            for v in requested {
                let Some(id) = v.as_str() else {
                    return Err(json!({
                        "type": "invalidArguments",
                        "description": "ids must be an array of strings",
                    }));
                };
                match account_id_for_principal(id).and_then(|a| fixture.account(a)) {
                    Some(a) => list.push(serialize_principal(a)),
                    None => not_found.push(Value::String(id.to_string())),
                }
            }
            (Value::Array(list), Value::Array(not_found))
        }
        Some(_) => {
            return Err(json!({
                "type": "invalidArguments",
                "description": "ids must be an array or null",
            }));
        }
    };

    let mut out = Map::new();
    out.insert(
        "accountId".to_string(),
        Value::String(account_id.to_string()),
    );
    // Principals are server-wide rather than per-account state, so the
    // token is the process-wide one every session-level site reports.
    out.insert(
        "state".to_string(),
        Value::String(fixture.primary_state().to_string()),
    );
    out.insert("list".to_string(), list);
    out.insert("notFound".to_string(), not_found);
    Ok(Value::Object(out))
}

/// One account as its owner principal. `Account::name` is the account's
/// email address by fixture convention (the same field Graph's profile
/// projects as `mail` / `userPrincipalName`), so `name` and `email`
/// coincide - the consumer reads `email`.
fn serialize_principal(a: &Account) -> Value {
    let mut obj = Map::new();
    obj.insert("id".to_string(), Value::String(principal_id_for(&a.id)));
    obj.insert("type".to_string(), Value::String("individual".to_string()));
    obj.insert("name".to_string(), Value::String(a.name.clone()));
    obj.insert("email".to_string(), Value::String(a.name.clone()));
    Value::Object(obj)
}
