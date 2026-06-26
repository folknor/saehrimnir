//! Synthetic data pools for `bulk_emails` and friends.
//!
//! Distilled from `<ratatoskr>/crates/dev-seed/src/templates.rs` and
//! pruned to the minimum that produces plausible-looking emails. The
//! pools are smaller than dev-seed's because saehrimnir's purpose is
//! "byte-stable scenario data," not "realistic-looking inbox for a
//! demo." Extend when a fixture demands it.

use rand::RngExt;
use rand::rngs::SmallRng;

pub const FIRST_NAMES: &[&str] = &[
    "Alice", "Bob", "Carol", "Dave", "Eve", "Frank", "Grace", "Heidi", "Ivan", "Judy", "Kris",
    "Liam", "Mallory", "Noor", "Olivia", "Peggy", "Quinn", "Rupert", "Sybil", "Trent", "Uma",
    "Victor", "Wendy", "Xander", "Yael", "Zara",
];

pub const LAST_NAMES: &[&str] = &[
    "Anderson", "Bennett", "Castro", "Diaz", "Edwards", "Fischer", "Garcia", "Hansen", "Iversen",
    "Johansen", "Kowalski", "Larsen", "Mendez", "Nakamura", "Olsen", "Petrov", "Quinn", "Roberts",
    "Singh", "Tanaka", "Ueda", "Vega", "Watanabe", "Xu", "Yamamoto", "Zhao",
];

pub const DOMAINS: &[&str] = &[
    "example.com",
    "example.org",
    "example.net",
    "test.local",
    "saehrimnir.test",
];

pub const PROJECTS: &[&str] = &[
    "Atlas",
    "Beacon",
    "Compass",
    "Delta",
    "Echo",
    "Forge",
    "Granite",
    "Horizon",
    "Iris",
    "Jetstream",
    "Keystone",
    "Lighthouse",
    "Mercury",
    "Nexus",
    "Orbit",
    "Pinnacle",
    "Quantum",
    "Relay",
    "Spectrum",
    "Titan",
];

pub const TEAMS: &[&str] = &[
    "engineering",
    "platform",
    "product",
    "design",
    "infrastructure",
    "data",
    "security",
    "mobile",
    "frontend",
    "backend",
    "devops",
    "growth",
];

pub const TOPICS: &[&str] = &[
    "microservices",
    "GraphQL",
    "Rust",
    "WebAssembly",
    "machine learning",
    "edge computing",
    "observability",
    "TypeScript",
    "Kubernetes",
    "CI/CD",
    "database sharding",
    "caching strategy",
    "API versioning",
    "OAuth 2.0",
    "event sourcing",
    "container security",
    "performance tuning",
];

pub const SUBJECT_TEMPLATES: &[&str] = &[
    "{project} planning notes",
    "Re: {project} kickoff",
    "{topic} review for {project}",
    "Weekly {team} sync",
    "{project} status update",
    "Question about {topic}",
    "Follow-up on {project}",
    "{team} retro action items",
    "{topic} proposal",
    "{project}: design review",
];

pub const BODY_TEMPLATES: &[&str] = &[
    "Hi {first_name},\r\n\r\nQuick update on {project}: the {team} team has been making progress on {topic}. Let's sync this week to align on next steps.\r\n\r\nThanks,\r\n{first_name_b}",
    "Team,\r\n\r\nWanted to share some thoughts on {topic}. I think {project} could benefit from adopting this approach. Happy to discuss in our next standup.\r\n\r\nBest,\r\n{first_name}",
    "Hey,\r\n\r\nThe {topic} work for {project} is wrapping up nicely. {team} has flagged a few open questions - let me know when you have time to chat.\r\n\r\nCheers,\r\n{first_name}",
    "{first_name},\r\n\r\nFollowing up on our conversation about {project}. Let me know if {topic} is still the right direction or if {team} wants to pivot.\r\n\r\nTalk soon,\r\n{first_name_b}",
];

/// Fill a template's `{placeholder}` tokens by drawing from the
/// matching pool. Unknown placeholders pass through verbatim so a
/// typo is visible in the output rather than silently dropped.
pub fn fill_template(template: &str, rng: &mut SmallRng) -> String {
    let mut out = String::with_capacity(template.len() * 2);
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '{' {
            out.push(c);
            continue;
        }
        let mut name = String::new();
        for next in chars.by_ref() {
            if next == '}' {
                break;
            }
            name.push(next);
        }
        match name.as_str() {
            "project" => out.push_str(pick(rng, PROJECTS)),
            "team" => out.push_str(pick(rng, TEAMS)),
            "topic" => out.push_str(pick(rng, TOPICS)),
            "first_name" | "first_name_a" => out.push_str(pick(rng, FIRST_NAMES)),
            "first_name_b" => out.push_str(pick(rng, FIRST_NAMES)),
            "last_name" => out.push_str(pick(rng, LAST_NAMES)),
            "domain" => out.push_str(pick(rng, DOMAINS)),
            other => {
                out.push('{');
                out.push_str(other);
                out.push('}');
            }
        }
    }
    out
}

/// Generate a `Name <email@domain>`-shaped string.
pub fn pick_address(rng: &mut SmallRng) -> (String, String) {
    let first = pick(rng, FIRST_NAMES);
    let last = pick(rng, LAST_NAMES);
    let domain = pick(rng, DOMAINS);
    let display = format!("{first} {last}");
    let local = format!(
        "{}.{}",
        first.to_ascii_lowercase(),
        last.to_ascii_lowercase()
    );
    (display, format!("{local}@{domain}"))
}

fn pick<'a, T>(rng: &mut SmallRng, slice: &'a [T]) -> &'a T {
    &slice[rng.random_range(0..slice.len())]
}
