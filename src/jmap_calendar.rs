//! JMAP calendar handlers (`urn:ietf:params:jmap:calendars`).
//!
//! Wire types follow RFC 8984 (JSCalendar) for events and the JMAP
//! Calendar specification for the wrapping object. The shared
//! `Fixture` already carries `Calendar` / `Event` because the Graph
//! and CalDAV listeners project them; this module adds the JMAP
//! projection on top.
//!
//! Mutations land via `Fixture::mutate` and append the same
//! `event_*` transitions Graph and CalDAV write today, so a JMAP
//! `CalendarEvent/set` surfaces in a follow-up Graph
//! `calendarView/delta` and a CalDAV CTag bumps the next time the
//! caller polls.
//!
//! `Calendar/changes` uses the raw seed-or-current state machine
//! (calendars are a near-static fixture surface; mutating them
//! through JMAP is tracked separately). `CalendarEvent/changes`
//! walks the change_log via `Fixture::event_delta_since`, which
//! takes a `calendar_id` filter; JMAP doesn't carry one, so we walk
//! the union of every fixture calendar and merge the resulting
//! delta sets.

use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};
use serde_json::{Map, Value, json};

use crate::fixture::{Address, Calendar, Event, Fixture, MutationDiff};

// ── Calendar/get ────────────────────────────────────────────────────

pub(crate) fn calendar_get(fixture: &Fixture, args: &Value) -> Result<Value, Value> {
    let account_id = require_account(fixture, args)?;

    let (list, not_found) = match args.get("ids") {
        None | Some(Value::Null) => {
            let all = fixture
                .calendars_for(account_id)
                .map(serialize_calendar)
                .collect::<Vec<_>>();
            (Value::Array(all), Value::Array(vec![]))
        }
        Some(Value::Array(requested)) => {
            let mut list = Vec::with_capacity(requested.len());
            let mut not_found = Vec::new();
            for v in requested {
                let Some(id) = v.as_str() else {
                    return Err(invalid_args("ids must be an array of strings"));
                };
                match fixture.calendars_for(account_id).find(|c| c.id == id) {
                    Some(c) => list.push(serialize_calendar(c)),
                    None => not_found.push(Value::String(id.to_string())),
                }
            }
            (Value::Array(list), Value::Array(not_found))
        }
        Some(_) => return Err(invalid_args("ids must be an array or null")),
    };

    let mut out = Map::new();
    out.insert("accountId".into(), Value::String(account_id.to_string()));
    out.insert("state".into(), Value::String(fixture.state.clone()));
    out.insert("list".into(), list);
    out.insert("notFound".into(), not_found);
    Ok(Value::Object(out))
}

fn serialize_calendar(c: &Calendar) -> Value {
    let mut obj = Map::new();
    obj.insert("id".into(), Value::String(c.id.clone()));
    obj.insert("name".into(), Value::String(c.name.clone()));
    obj.insert(
        "color".into(),
        c.color
            .clone()
            .map(Value::String)
            .unwrap_or(Value::Null),
    );
    obj.insert("isDefault".into(), Value::Bool(c.is_default));
    obj.insert("isVisible".into(), Value::Bool(true));
    obj.insert("isSubscribed".into(), Value::Bool(true));
    obj.insert("sortOrder".into(), Value::Number(0.into()));
    obj.insert("timeZone".into(), Value::String("UTC".into()));
    obj.insert("myRights".into(), my_rights_all_true());
    Value::Object(obj)
}

fn my_rights_all_true() -> Value {
    let mut r = Map::new();
    for k in [
        "mayReadFreeBusy",
        "mayReadItems",
        "mayWriteAll",
        "mayWriteOwn",
        "mayUpdatePrivate",
        "mayRSVP",
        "mayShare",
        "mayDelete",
    ] {
        r.insert(k.into(), Value::Bool(true));
    }
    Value::Object(r)
}

// ── Calendar/changes ────────────────────────────────────────────────

pub(crate) fn calendar_changes(fixture: &Fixture, args: &Value) -> Result<Value, Value> {
    let account_id = require_account(fixture, args)?;
    let since_state = require_since_state(args)?;
    // Walk the per-account calendar delta. MKCALENDAR / DELETE on a
    // collection record `calendar_*` transitions; on a fixture that
    // hasn't seen any of those, every retained state yields the
    // empty delta.
    let delta = fixture
        .calendar_delta_since_account(since_state, account_id)
        .ok_or_else(|| {
            json!({
                "type": "cannotCalculateChanges",
                "description": format!(
                    "sinceState {since_state:?} is not a known fixture state \
                     (older than the seed or evicted from the bounded change log)"
                ),
            })
        })?;
    Ok(json!({
        "accountId": account_id,
        "oldState": since_state,
        "newState": fixture.state,
        "hasMoreChanges": false,
        "created": delta.created,
        "updated": delta.updated,
        "destroyed": delta.destroyed,
    }))
}

// ── CalendarEvent/get ───────────────────────────────────────────────

pub(crate) fn calendar_event_get(fixture: &Fixture, args: &Value) -> Result<Value, Value> {
    let account_id = require_account(fixture, args)?;

    let (list, not_found) = match args.get("ids") {
        None | Some(Value::Null) => {
            let all = fixture
                .events_for(account_id)
                .map(serialize_event)
                .collect::<Vec<_>>();
            (Value::Array(all), Value::Array(vec![]))
        }
        Some(Value::Array(requested)) => {
            let mut list = Vec::with_capacity(requested.len());
            let mut not_found = Vec::new();
            for v in requested {
                let Some(id) = v.as_str() else {
                    return Err(invalid_args("ids must be an array of strings"));
                };
                match fixture.events_for(account_id).find(|e| e.id == id) {
                    Some(e) => list.push(serialize_event(e)),
                    None => not_found.push(Value::String(id.to_string())),
                }
            }
            (Value::Array(list), Value::Array(not_found))
        }
        Some(_) => return Err(invalid_args("ids must be an array or null")),
    };

    let mut out = Map::new();
    out.insert("accountId".into(), Value::String(account_id.to_string()));
    out.insert("state".into(), Value::String(fixture.state.clone()));
    out.insert("list".into(), list);
    out.insert("notFound".into(), not_found);
    Ok(Value::Object(out))
}

/// Project a fixture `Event` to a JSCalendar object.
///
/// Field set covers what `crates/jmap/src/calendar_sync/payload.rs`
/// in ratatoskr reads back: `id`, `uid`, `calendarIds`, `title`,
/// `description`, `start`, `duration`, `showWithoutTime`, `timeZone`,
/// `status`, `locations`, `participants`. Everything else (alerts,
/// recurrenceRules, recurrenceOverrides, virtualLocations) is omitted
/// in v0; ratatoskr's persist path tolerates absence on every field.
fn serialize_event(e: &Event) -> Value {
    let mut out = Map::new();
    out.insert("@type".into(), Value::String("Event".into()));
    out.insert("id".into(), Value::String(e.id.clone()));
    out.insert("uid".into(), Value::String(e.id.clone()));

    let mut calendar_ids = Map::new();
    calendar_ids.insert(e.calendar_id.clone(), Value::Bool(true));
    out.insert("calendarIds".into(), Value::Object(calendar_ids));

    out.insert("title".into(), Value::String(e.subject.clone()));
    if let Some(d) = &e.body_text {
        out.insert("description".into(), Value::String(d.clone()));
    }

    let (start_str, duration_str) = format_jscalendar_times(e.start, e.end, e.is_all_day);
    out.insert("start".into(), Value::String(start_str));
    out.insert("duration".into(), Value::String(duration_str));
    out.insert("timeZone".into(), Value::String("UTC".into()));
    if e.is_all_day {
        out.insert("showWithoutTime".into(), Value::Bool(true));
    }
    out.insert("status".into(), Value::String("confirmed".into()));

    if let Some(loc) = &e.location {
        let mut locs = Map::new();
        locs.insert(
            "loc1".into(),
            json!({"@type": "Location", "name": loc}),
        );
        out.insert("locations".into(), Value::Object(locs));
    }

    let mut participants = Map::new();
    if let Some(org) = &e.organizer {
        participants.insert("org".into(), serialize_participant(org, true));
    }
    for (idx, att) in e.attendees.iter().enumerate() {
        participants.insert(
            format!("att{}", idx + 1),
            serialize_participant(att, false),
        );
    }
    if !participants.is_empty() {
        out.insert("participants".into(), Value::Object(participants));
    }

    if let Some(rrule) = &e.recurrence_rule {
        let parsed = crate::recurrence::ParsedRule::parse(rrule);
        if let Some(rule_obj) = jscalendar_recurrence_rule(&parsed) {
            out.insert("recurrenceRules".into(), Value::Array(vec![rule_obj]));
        }
    }

    Value::Object(out)
}

/// Translate a parsed RRULE into a single JSCalendar
/// `RecurrenceRule` object. JSCalendar (RFC 8984 §4.3.3) shape:
/// `{ @type: RecurrenceRule, frequency, interval?, byDay?,
///    byMonthDay?, byMonth?, count?, until? }`. `byDay` is an
/// array of `NDay` objects (`{@type: NDay, day, nthOfPeriod?}`).
/// Returns None when FREQ is missing.
fn jscalendar_recurrence_rule(rule: &crate::recurrence::ParsedRule) -> Option<Value> {
    let freq = rule.freq?;
    let mut obj = Map::new();
    obj.insert("@type".into(), Value::String("RecurrenceRule".into()));
    obj.insert("frequency".into(), Value::String(freq.jscalendar().into()));
    if let Some(n) = rule.interval
        && n > 1
    {
        obj.insert("interval".into(), Value::Number(n.into()));
    }
    if !rule.by_day.is_empty() {
        let by_day: Vec<Value> = rule
            .by_day
            .iter()
            .map(|d| {
                let mut nd = Map::new();
                nd.insert("@type".into(), Value::String("NDay".into()));
                nd.insert("day".into(), Value::String(d.weekday.jscalendar().into()));
                if let Some(ord) = d.ordinal {
                    nd.insert("nthOfPeriod".into(), Value::Number(ord.into()));
                }
                Value::Object(nd)
            })
            .collect();
        obj.insert("byDay".into(), Value::Array(by_day));
    }
    if !rule.by_month_day.is_empty() {
        let by_md: Vec<Value> = rule
            .by_month_day
            .iter()
            .map(|d| Value::Number((*d).into()))
            .collect();
        obj.insert("byMonthDay".into(), Value::Array(by_md));
    }
    if !rule.by_month.is_empty() {
        // RFC 8984 expects month strings (`"1"`, ..., `"12"`,
        // optionally suffixed with `"L"` for leap). v0 emits the
        // numeric form as a string per spec.
        let by_m: Vec<Value> = rule
            .by_month
            .iter()
            .map(|m| Value::String(m.to_string()))
            .collect();
        obj.insert("byMonth".into(), Value::Array(by_m));
    }
    if let Some(c) = rule.count {
        obj.insert("count".into(), Value::Number(c.into()));
    }
    if let Some(u) = rule.until {
        // JSCalendar `until` is a LocalDateTime (no Z suffix).
        // Drop the trailing Z; the canonical timeZone for our
        // events is UTC anyway.
        obj.insert(
            "until".into(),
            Value::String(u.format("%Y-%m-%dT%H:%M:%S").to_string()),
        );
    }
    Some(Value::Object(obj))
}

/// Inverse of [`jscalendar_recurrence_rule`]. Read JSCalendar's
/// `recurrenceRules: [{...}]` array, take the first rule (v0
/// authors at most one), and synthesise an RRULE string. Returns
/// None when the array is empty, the first rule lacks `frequency`,
/// or `frequency` is outside the daily / weekly / monthly / yearly
/// set v0 models.
///
/// JSCalendar `recurrenceOverrides` are out of scope on the write
/// path (Stage 1 of recurrence read-side already documents the
/// omission); EXDATEs the fixture wants to author go through the
/// static `[[event]]` shape.
fn parse_jscalendar_recurrence_rules(v: &Value) -> Option<String> {
    use crate::recurrence::{ByDay, Frequency, ParsedRule, Weekday};
    let arr = v.as_array()?;
    let first = arr.first()?.as_object()?;
    let freq = match first.get("frequency")?.as_str()? {
        "daily" => Frequency::Daily,
        "weekly" => Frequency::Weekly,
        "monthly" => Frequency::Monthly,
        "yearly" => Frequency::Yearly,
        _ => return None,
    };
    let mut rule = ParsedRule {
        freq: Some(freq),
        ..Default::default()
    };
    if let Some(n) = first
        .get("interval")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
    {
        rule.interval = Some(n);
    }
    if let Some(by_day) = first.get("byDay").and_then(Value::as_array) {
        rule.by_day = by_day
            .iter()
            .filter_map(|d| {
                let o = d.as_object()?;
                let weekday = Weekday::parse_short(o.get("day")?.as_str()?)?;
                let ordinal = o
                    .get("nthOfPeriod")
                    .and_then(Value::as_i64)
                    .and_then(|n| i32::try_from(n).ok());
                Some(ByDay { ordinal, weekday })
            })
            .collect();
    }
    if let Some(by_md) = first.get("byMonthDay").and_then(Value::as_array) {
        rule.by_month_day = by_md
            .iter()
            .filter_map(|v| v.as_i64().and_then(|n| i8::try_from(n).ok()))
            .collect();
    }
    if let Some(by_m) = first.get("byMonth").and_then(Value::as_array) {
        // RFC 8984 emits month strings (`"1"`, ..., `"12"`,
        // optional `"L"` suffix). The L suffix is rare and v0
        // doesn't model leap months; strip it and parse the
        // numeric portion.
        rule.by_month = by_m
            .iter()
            .filter_map(|v| v.as_str())
            .map(|s| s.trim_end_matches('L'))
            .filter_map(|s| s.parse::<u8>().ok())
            .collect();
    }
    if let Some(c) = first
        .get("count")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
    {
        rule.count = Some(c);
    }
    if let Some(u) = first.get("until").and_then(Value::as_str)
        && let Ok(dt) = chrono::NaiveDateTime::parse_from_str(u, "%Y-%m-%dT%H:%M:%S")
    {
        rule.until = Some(dt.and_utc());
    }
    crate::recurrence::format_rrule(&rule)
}

fn serialize_participant(addr: &Address, is_owner: bool) -> Value {
    let mut p = Map::new();
    p.insert("@type".into(), Value::String("Participant".into()));
    if let Some(n) = &addr.name {
        p.insert("name".into(), Value::String(n.clone()));
    }
    p.insert("email".into(), Value::String(addr.email.clone()));
    p.insert(
        "sendTo".into(),
        json!({"imip": format!("mailto:{}", addr.email)}),
    );
    let mut roles = Map::new();
    if is_owner {
        roles.insert("owner".into(), Value::Bool(true));
        roles.insert("attendee".into(), Value::Bool(true));
    } else {
        roles.insert("attendee".into(), Value::Bool(true));
    }
    p.insert("roles".into(), Value::Object(roles));
    p.insert("participationStatus".into(), Value::String("needs-action".into()));
    Value::Object(p)
}

// ── CalendarEvent/changes ───────────────────────────────────────────

pub(crate) fn calendar_event_changes(
    fixture: &Fixture,
    args: &Value,
) -> Result<Value, Value> {
    let account_id = require_account(fixture, args)?;
    let since_state = require_since_state(args)?;

    // JMAP `CalendarEvent/changes` carries no per-calendar filter,
    // so use the cross-calendar walker. Per-calendar dominance
    // (created+destroyed cancels) already applied by
    // `apply_dominance_and_dedup` inside `delta_since`.
    let delta = fixture.event_delta_since_any(since_state).ok_or_else(|| {
        json!({
            "type": "cannotCalculateChanges",
            "description": format!(
                "sinceState {since_state:?} is not a known fixture state \
                 (older than the seed or evicted from the bounded change log)"
            ),
        })
    })?;

    Ok(json!({
        "accountId": account_id,
        "oldState": since_state,
        "newState": fixture.state,
        "hasMoreChanges": false,
        "created": delta.created,
        "updated": delta.updated,
        "destroyed": delta.destroyed,
    }))
}

// ── CalendarEvent/set ───────────────────────────────────────────────

pub(crate) fn calendar_event_set(
    fixture: &mut Fixture,
    args: &Value,
) -> Result<Value, Value> {
    let account_id = require_account(fixture, args)?.to_string();
    if let Some(if_in_state) = args.get("ifInState").and_then(Value::as_str)
        && if_in_state != fixture.state
    {
        return Err(json!({
            "type": "stateMismatch",
            "description": format!(
                "ifInState {if_in_state:?} does not match current state {:?}",
                fixture.state,
            ),
        }));
    }

    let creates = args
        .get("create")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let updates = args
        .get("update")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let destroys: Vec<String> = args
        .get("destroy")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let mut created_out = Map::new();
    let mut not_created = Map::new();
    let mut updated_out = Map::new();
    let mut not_updated = Map::new();
    let mut destroyed_out: Vec<String> = Vec::new();
    let mut not_destroyed = Map::new();

    let trans = fixture.mutate(|fix| {
        let mut diff = MutationDiff::default();

        for (client_id, body) in &creates {
            match build_event_from_create(fix, body) {
                Ok((server_id, event)) => {
                    fix.events.push(event);
                    diff.event_created.push(server_id.clone());
                    created_out.insert(
                        client_id.clone(),
                        json!({"id": server_id, "uid": server_id}),
                    );
                }
                Err(err) => {
                    not_created.insert(client_id.clone(), err);
                }
            }
        }

        for (id, patch) in &updates {
            let Some(idx) = fix.events.iter().position(|e| &e.id == id) else {
                not_updated.insert(id.clone(), set_error("notFound", "no such event"));
                continue;
            };
            let mut clone = fix.events[idx].clone();
            match apply_event_patch(&mut clone, patch) {
                Ok(()) => {
                    fix.events[idx] = clone;
                    diff.event_updated.push(id.clone());
                    updated_out.insert(id.clone(), Value::Null);
                }
                Err(err) => {
                    not_updated.insert(id.clone(), err);
                }
            }
        }

        for id in &destroys {
            let Some(idx) = fix.events.iter().position(|e| &e.id == id) else {
                not_destroyed.insert(id.clone(), set_error("notFound", "no such event"));
                continue;
            };
            let parent = fix.events[idx].calendar_id.clone();
            fix.events.remove(idx);
            diff.event_destroyed.push(id.clone());
            diff.event_destroyed_parents.push(parent);
            destroyed_out.push(id.clone());
        }

        diff
    });

    Ok(json!({
        "accountId": account_id,
        "oldState": trans.from_state,
        "newState": trans.to_state,
        "created": if created_out.is_empty() { Value::Null } else { Value::Object(created_out) },
        "notCreated": if not_created.is_empty() { Value::Null } else { Value::Object(not_created) },
        "updated": if updated_out.is_empty() { Value::Null } else { Value::Object(updated_out) },
        "notUpdated": if not_updated.is_empty() { Value::Null } else { Value::Object(not_updated) },
        "destroyed": if destroyed_out.is_empty() { Value::Null } else { Value::Array(destroyed_out.into_iter().map(Value::String).collect()) },
        "notDestroyed": if not_destroyed.is_empty() { Value::Null } else { Value::Object(not_destroyed) },
    }))
}

fn build_event_from_create(fix: &mut Fixture, body: &Value) -> Result<(String, Event), Value> {
    let obj = body
        .as_object()
        .ok_or_else(|| set_error("invalidProperties", "create body must be an object"))?;

    let calendar_id = obj
        .get("calendarIds")
        .and_then(Value::as_object)
        .and_then(|m| {
            m.iter()
                .find(|(_, v)| v.as_bool() == Some(true))
                .map(|(k, _)| k.clone())
        })
        .ok_or_else(|| set_error("invalidProperties", "missing calendarIds"))?;

    if !fix.calendars.iter().any(|c| c.id == calendar_id) {
        return Err(set_error(
            "invalidProperties",
            &format!("unknown calendar {calendar_id:?}"),
        ));
    }

    let title = obj
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let description = obj
        .get("description")
        .and_then(Value::as_str)
        .map(str::to_string);

    let is_all_day = obj
        .get("showWithoutTime")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let start_str = obj
        .get("start")
        .and_then(Value::as_str)
        .ok_or_else(|| set_error("invalidProperties", "missing start"))?;
    let duration_str = obj
        .get("duration")
        .and_then(Value::as_str)
        .unwrap_or(if is_all_day { "P1D" } else { "PT1H" });
    let (start, end) = parse_jscalendar_times(start_str, duration_str, is_all_day)
        .ok_or_else(|| set_error("invalidProperties", "could not parse start/duration"))?;

    let location = obj
        .get("locations")
        .and_then(Value::as_object)
        .and_then(|m| m.values().next())
        .and_then(|v| v.get("name"))
        .and_then(Value::as_str)
        .map(str::to_string);

    let (organizer, attendees) = parse_participants(obj.get("participants"));

    let server_id = fix.mint_event_id();
    let account_id = fix
        .calendars
        .iter()
        .find(|c| c.id == calendar_id)
        .map(|c| c.account_id.clone())
        .unwrap_or_else(|| fix.primary_account().id.clone());
    let recurrence_rule = obj
        .get("recurrenceRules")
        .and_then(parse_jscalendar_recurrence_rules);
    Ok((
        server_id.clone(),
        Event {
            id: server_id,
            account_id,
            calendar_id,
            subject: title,
            body_preview: None,
            body_text: description,
            start,
            end,
            location,
            organizer,
            attendees,
            is_all_day,
            recurrence_rule,
            // JSCalendar models exceptions via `recurrenceOverrides`
            // rather than EXDATE arrays. v0 doesn't model those on
            // the write path; the fixture's static authoring shape
            // keeps the field accessible if needed.
            recurrence_exdates: vec![],
        },
    ))
}

fn apply_event_patch(event: &mut Event, body: &Value) -> Result<(), Value> {
    let obj = body
        .as_object()
        .ok_or_else(|| set_error("invalidProperties", "patch must be an object"))?;
    for (k, v) in obj {
        match k.as_str() {
            "title" => event.subject = v.as_str().unwrap_or("").to_string(),
            "description" => {
                event.body_text = v.as_str().map(str::to_string);
            }
            "showWithoutTime" => {
                event.is_all_day = v.as_bool().unwrap_or(false);
            }
            "start" | "duration" => {
                // Defer; handled below after the loop so we don't
                // half-apply when only one of the two is patched.
            }
            "locations" => {
                event.location = v
                    .as_object()
                    .and_then(|m| m.values().next())
                    .and_then(|val| val.get("name"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            "participants" => {
                let (org, atts) = parse_participants(Some(v));
                event.organizer = org;
                event.attendees = atts;
            }
            "calendarIds" => {
                if let Some(new_cal) = v
                    .as_object()
                    .and_then(|m| {
                        m.iter()
                            .find(|(_, vv)| vv.as_bool() == Some(true))
                            .map(|(k, _)| k.clone())
                    })
                {
                    event.calendar_id = new_cal;
                }
            }
            "recurrenceRules" => {
                // null / empty array clears recurrence; a
                // non-empty array replaces it with the first
                // rule's RRULE projection (v0 authors at most
                // one rule per event).
                if v.is_null() {
                    event.recurrence_rule = None;
                    event.recurrence_exdates.clear();
                } else {
                    event.recurrence_rule = parse_jscalendar_recurrence_rules(v);
                    event.recurrence_exdates.clear();
                }
            }
            _ => {
                // Quietly ignore properties v0 doesn't model. Real
                // JMAP servers tolerate extension keys; rejecting
                // unknown ones would break harmless pass-throughs.
            }
        }
    }
    // Recompute start/end whenever any of `start` / `duration` /
    // `showWithoutTime` is in the patch. The all-day case matters
    // even when start/duration are absent: flipping to all-day
    // (or back) without re-deriving leaves the event with mixed
    // mode (`is_all_day = true` but a `T`-bearing start/end).
    if obj.contains_key("start")
        || obj.contains_key("duration")
        || obj.contains_key("showWithoutTime")
    {
        let start_str = obj
            .get("start")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| {
                if event.is_all_day {
                    event.start.format("%Y-%m-%d").to_string()
                } else {
                    event.start.format("%Y-%m-%dT%H:%M:%S").to_string()
                }
            });
        let secs = (event.end - event.start).num_seconds().max(0);
        let default_duration = if event.is_all_day {
            format!("P{}D", (secs / 86400).max(1))
        } else {
            format_duration_iso8601(secs)
        };
        let duration_str = obj
            .get("duration")
            .and_then(Value::as_str)
            .unwrap_or(&default_duration);
        let (start, end) = parse_jscalendar_times(&start_str, duration_str, event.is_all_day)
            .ok_or_else(|| {
                set_error("invalidProperties", "could not parse start/duration")
            })?;
        event.start = start;
        event.end = end;
    }
    Ok(())
}

fn parse_participants(v: Option<&Value>) -> (Option<Address>, Vec<Address>) {
    let Some(obj) = v.and_then(Value::as_object) else {
        return (None, vec![]);
    };
    let mut organizer: Option<Address> = None;
    let mut attendees: Vec<Address> = Vec::new();
    for (_id, p) in obj {
        let Some(p_obj) = p.as_object() else { continue };
        let email = p_obj
            .get("sendTo")
            .and_then(|s| s.as_object())
            .and_then(|s| s.get("imip"))
            .and_then(Value::as_str)
            .map(|e| e.strip_prefix("mailto:").unwrap_or(e).to_string())
            .or_else(|| {
                p_obj
                    .get("email")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });
        let Some(email) = email else { continue };
        let name = p_obj
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string);
        let is_owner = p_obj
            .get("roles")
            .and_then(Value::as_object)
            .is_some_and(|r| r.contains_key("owner"));
        let addr = Address { email, name };
        if is_owner && organizer.is_none() {
            organizer = Some(addr);
        } else {
            attendees.push(addr);
        }
    }
    (organizer, attendees)
}

// ── JSCalendar time helpers ─────────────────────────────────────────

fn format_jscalendar_times(
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    is_all_day: bool,
) -> (String, String) {
    if is_all_day {
        let start_str = start.format("%Y-%m-%d").to_string();
        let days = ((end - start).num_seconds() / 86400).max(1);
        (start_str, format!("P{days}D"))
    } else {
        let start_str = start.format("%Y-%m-%dT%H:%M:%S").to_string();
        let secs = (end - start).num_seconds().max(0);
        (start_str, format_duration_iso8601(secs))
    }
}

fn format_duration_iso8601(mut secs: i64) -> String {
    if secs <= 0 {
        return "PT0S".into();
    }
    let mut out = String::from("P");
    let days = secs / 86400;
    if days > 0 {
        out.push_str(&format!("{days}D"));
        secs %= 86400;
    }
    if secs > 0 {
        out.push('T');
        let hours = secs / 3600;
        if hours > 0 {
            out.push_str(&format!("{hours}H"));
            secs %= 3600;
        }
        let minutes = secs / 60;
        if minutes > 0 {
            out.push_str(&format!("{minutes}M"));
            secs %= 60;
        }
        if secs > 0 {
            out.push_str(&format!("{secs}S"));
        }
    }
    out
}

fn parse_jscalendar_times(
    start: &str,
    duration: &str,
    is_all_day: bool,
) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let start_dt = if is_all_day {
        let date = NaiveDate::parse_from_str(start, "%Y-%m-%d").ok()?;
        Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0)?)
    } else {
        let naive = NaiveDateTime::parse_from_str(start, "%Y-%m-%dT%H:%M:%S")
            .or_else(|_| {
                DateTime::parse_from_rfc3339(start).map(|dt| dt.with_timezone(&Utc).naive_utc())
            })
            .ok()?;
        Utc.from_utc_datetime(&naive)
    };
    let secs = parse_iso8601_duration(duration);
    Some((start_dt, start_dt + chrono::Duration::seconds(secs)))
}

fn parse_iso8601_duration(s: &str) -> i64 {
    let mut total: i64 = 0;
    let mut in_time = false;
    let mut buf = String::new();
    for ch in s.chars() {
        match ch {
            'P' => {}
            'T' => in_time = true,
            'W' => {
                if let Ok(n) = buf.parse::<i64>() {
                    total += n * 7 * 86400;
                }
                buf.clear();
            }
            'D' => {
                if let Ok(n) = buf.parse::<i64>() {
                    total += n * 86400;
                }
                buf.clear();
            }
            'H' if in_time => {
                if let Ok(n) = buf.parse::<i64>() {
                    total += n * 3600;
                }
                buf.clear();
            }
            'M' if in_time => {
                if let Ok(n) = buf.parse::<i64>() {
                    total += n * 60;
                }
                buf.clear();
            }
            'S' if in_time => {
                if let Ok(n) = buf.parse::<i64>() {
                    total += n;
                }
                buf.clear();
            }
            c if c.is_ascii_digit() => buf.push(c),
            _ => {}
        }
    }
    total
}

// ── Shared helpers ──────────────────────────────────────────────────

fn require_account<'a>(fixture: &'a Fixture, args: &'a Value) -> Result<&'a str, Value> {
    let account_id = args
        .get("accountId")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_args("missing accountId"))?;
    if fixture.account(account_id).is_none() {
        return Err(json!({
            "type": "accountNotFound",
            "description": format!("account {account_id:?} not found"),
        }));
    }
    Ok(account_id)
}

fn require_since_state(args: &Value) -> Result<&str, Value> {
    args.get("sinceState")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_args("missing sinceState"))
}

fn invalid_args(description: &str) -> Value {
    json!({"type": "invalidArguments", "description": description})
}

fn set_error(kind: &str, description: &str) -> Value {
    json!({"type": kind, "description": description})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso8601_duration_round_trips() {
        assert_eq!(parse_iso8601_duration("PT1H"), 3600);
        assert_eq!(parse_iso8601_duration("PT1H30M"), 5400);
        assert_eq!(parse_iso8601_duration("P1D"), 86400);
        assert_eq!(parse_iso8601_duration("P1DT2H"), 93600);
        assert_eq!(format_duration_iso8601(3600), "PT1H");
        assert_eq!(format_duration_iso8601(5400), "PT1H30M");
        assert_eq!(format_duration_iso8601(86400), "P1D");
    }

    #[test]
    fn jscalendar_times_round_trip_timed() {
        let start = Utc.with_ymd_and_hms(2026, 1, 15, 9, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2026, 1, 15, 9, 15, 0).unwrap();
        let (s, d) = format_jscalendar_times(start, end, false);
        assert_eq!(s, "2026-01-15T09:00:00");
        assert_eq!(d, "PT15M");
        let (s2, e2) = parse_jscalendar_times(&s, &d, false).unwrap();
        assert_eq!(s2, start);
        assert_eq!(e2, end);
    }

    #[test]
    fn jscalendar_times_round_trip_all_day() {
        let start = Utc.with_ymd_and_hms(2026, 1, 15, 0, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2026, 1, 16, 0, 0, 0).unwrap();
        let (s, d) = format_jscalendar_times(start, end, true);
        assert_eq!(s, "2026-01-15");
        assert_eq!(d, "P1D");
        let (s2, e2) = parse_jscalendar_times(&s, &d, true).unwrap();
        assert_eq!(s2, start);
        assert_eq!(e2, end);
    }
}
