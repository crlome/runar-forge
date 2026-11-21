//! Plans (PRDs) and icebox items, stored as ordinary memory entries.
//!
//! Both are documents that outlive a session and have to be shared with a
//! team, so they live in `memory_entries` and replicate through the existing
//! `sync_outbox` for free. A dedicated table would model status and phases
//! more directly, but the outbox is keyed on `memory_entries` alone — a new
//! table would have arrived unsynced, which is the one property this feature
//! could not do without.
//!
//! The cost of that choice is that identity, ordering and status all have to
//! be encoded in a `topic_key` and a tag set. This module owns those
//! conventions so the MCP tools and the CLI cannot drift apart on them.
//!
//! ## Key layout
//!
//! ```text
//! plan:<slug>                     the meta entry — title, overview, status
//! plan:<slug>:<nn>-<section>      one section, ordered by <nn>
//! plan:<slug>:<nn>-<section>--pNN one part of an oversized section
//! icebox:<slug>                   one backlog item
//! ```
//!
//! A section prefix query returns the parts in reading order because the
//! index is zero-padded and the part suffix extends the section key.
//! Status lives in tags rather than the key, so a status change supersedes
//! the entry in place instead of orphaning the old key.

use crate::librarian::{content_limit, MemoryLibrarian};
use crate::storage::StorageResult;
use crate::types::{EntryType, MemoryEntry, MemoryEntryInput, SaveAction};

pub const PLAN_PREFIX: &str = "plan:";
pub const ICEBOX_PREFIX: &str = "icebox:";

/// Refuse a generic save (`muninn_save`, `muninn_capture_passive`,
/// `runar save`) that would write into the plan store's space.
///
/// The storage topic-key supersede replaces whatever is live at a key, so a
/// decision saved at `plan:<slug>` replaced that plan's meta entry: the
/// plan showed the decision's title and read as an unstarted draft. The
/// types are refused for the same reason — a generic save cannot follow
/// the key and tag conventions this module owns. A key holding `<` or `>`
/// is refused too: keys are stored as given, so a `<private>` block inside
/// one would outlive the redaction applied to every other field.
///
/// The messages name the right tool and avoid every word the MCP breaker
/// reads as a storage failure.
pub fn check_generic_save(entry_type: EntryType, topic_key: Option<&str>) -> Result<(), String> {
    match entry_type {
        EntryType::Plan => {
            return Err("plans are written with muninn_plan_create and \
                        muninn_plan_save_section (CLI: `runar plan`), not with a generic save"
                .to_string())
        }
        EntryType::Icebox => {
            return Err("icebox items are written with muninn_icebox_add \
                        (CLI: `runar icebox`), not with a generic save"
                .to_string())
        }
        _ => {}
    }
    let Some(key) = topic_key else {
        return Ok(());
    };
    let folded = key.trim().to_ascii_lowercase();
    if folded.starts_with(PLAN_PREFIX) || folded.starts_with(ICEBOX_PREFIX) {
        return Err(format!(
            "topic key '{}' is in the `plan:` or `icebox:` scope, which belongs to the plan \
             tools (muninn_plan_create, muninn_icebox_add); use a scope such as \
             'feature:decision'",
            key.trim()
        ));
    }
    if key.contains(['<', '>']) {
        return Err(
            "a topic key cannot contain '<' or '>': keys are kept as given, so a \
                    private block inside one would not be redacted"
                .to_string(),
        );
    }
    Ok(())
}

/// Separator between a section key and its part number. Two dashes, because
/// a single dash is already legal inside a slugified section name and the
/// two would be indistinguishable.
const PART_SEP: &str = "--p";

// ── Slugs and keys ──────────────────────────────────────────────────

/// Kebab-case a title into a key-safe slug.
///
/// Unlike `mcp::suggest_topic_key` this keeps the whole title rather than
/// the first four words: a plan slug is an identity the user types back
/// (`runar plan show auth-rewrite`), so losing words to a truncation makes
/// two different plans collide. Length is capped instead, at a bound far
/// above any real title.
pub fn slugify(title: &str) -> String {
    let mut out = String::new();
    let mut last_dash = true; // leading dashes are suppressed
    for ch in title.to_lowercase().chars() {
        if ch.is_alphanumeric() {
            out.push(ch);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
        if out.chars().count() >= 64 {
            break;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        // A title of pure punctuation still needs an addressable key.
        "untitled".to_string()
    } else {
        out
    }
}

pub fn plan_meta_key(slug: &str) -> String {
    format!("{PLAN_PREFIX}{slug}")
}

/// Storage prefix covering a plan's meta entry and its sections.
///
/// Coarse on purpose: it also matches every longer slug (`plan:auth`
/// covers `plan:auth-flow`) and any non-plan row a generic save put under
/// the scope. Never act on what it returns without filtering through
/// `owned_by_plan`.
pub fn plan_scope_prefix(slug: &str) -> String {
    format!("{PLAN_PREFIX}{slug}")
}

pub fn plan_section_key(slug: &str, index: usize, name: &str) -> String {
    format!("{PLAN_PREFIX}{slug}:{index:02}-{}", slugify(name))
}

pub fn icebox_key(slug: &str) -> String {
    format!("{ICEBOX_PREFIX}{slug}")
}

/// Split `plan:<slug>[:<rest>]` into its slug and the section remainder.
/// Returns `None` for a key that is not a plan key at all.
pub fn parse_plan_key(topic_key: &str) -> Option<(&str, Option<&str>)> {
    let rest = topic_key.strip_prefix(PLAN_PREFIX)?;
    match rest.split_once(':') {
        Some((slug, section)) => Some((slug, Some(section))),
        None => Some((rest, None)),
    }
}

pub fn parse_icebox_key(topic_key: &str) -> Option<&str> {
    topic_key.strip_prefix(ICEBOX_PREFIX)
}

// ── Status ──────────────────────────────────────────────────────────

macro_rules! string_enum {
    ($name:ident, $default:ident, $( $variant:ident => $text:literal ),+ $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum $name { $( $variant, )+ }

        impl $name {
            pub fn as_str(&self) -> &'static str {
                match self { $( Self::$variant => $text, )+ }
            }
            pub fn parse(s: &str) -> Option<Self> {
                match s.trim().to_lowercase().as_str() {
                    $( $text => Some(Self::$variant), )+
                    _ => None,
                }
            }
            pub fn all() -> &'static [&'static str] {
                &[ $( $text, )+ ]
            }
        }

        impl Default for $name {
            fn default() -> Self { Self::$default }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

string_enum!(
    PlanStatus,
    Draft,
    Draft => "draft",
    Approved => "approved",
    InProgress => "in-progress",
    Completed => "completed",
    Abandoned => "abandoned",
);

string_enum!(
    PhaseStatus,
    Pending,
    Pending => "pending",
    InProgress => "in-progress",
    Done => "done",
);

string_enum!(
    IceboxStatus,
    Open,
    Open => "open",
    Promoted => "promoted",
    Dropped => "dropped",
);

/// A plan whose status is `completed` or `abandoned` must never be handed to
/// an agent as work to do. This is the guard behind "do not execute a plan
/// twice", and it belongs here rather than in the skill text, because a
/// skill file is editable and this rule is not advisory.
impl PlanStatus {
    pub fn is_closed(&self) -> bool {
        matches!(self, Self::Completed | Self::Abandoned)
    }
}

const STATUS_TAG: &str = "status:";
const PHASE_TAG: &str = "phase:";
const PHASE_STATUS_TAG: &str = "phase-status:";
const PLAN_REF_TAG: &str = "plan:";
const PROMOTED_TAG: &str = "promoted-to:";

fn tag_value<'a>(tags: &'a [String], prefix: &str) -> Option<&'a str> {
    tags.iter()
        .find_map(|t| t.strip_prefix(prefix))
        .filter(|v| !v.is_empty())
}

/// Replace any existing `prefix`-tagged value with `value`, preserving every
/// other tag. Status transitions go through here so a re-save never leaves
/// two contradictory status tags on one entry.
fn set_tag(tags: &mut Vec<String>, prefix: &str, value: &str) {
    tags.retain(|t| !t.starts_with(prefix));
    tags.push(format!("{prefix}{value}"));
}

/// Tags the plan store writes, and so may replace: identity, status and
/// phase. Every other tag on an entry (`redacted`, `truncated`, a caller's
/// own) survives a retag.
fn is_store_tag(tag: &str) -> bool {
    tag == "plan"
        || tag == "icebox"
        || [
            PLAN_REF_TAG,
            STATUS_TAG,
            PHASE_TAG,
            PHASE_STATUS_TAG,
            PROMOTED_TAG,
        ]
        .iter()
        .any(|prefix| tag.starts_with(prefix))
}

fn phase_rank(status: PhaseStatus) -> u8 {
    match status {
        PhaseStatus::Pending => 0,
        PhaseStatus::InProgress => 1,
        PhaseStatus::Done => 2,
    }
}

// ── Documents ───────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct PlanSection {
    pub topic_key: String,
    /// Section ordinal parsed back out of the key. Parts of one section all
    /// share an index.
    pub index: usize,
    pub title: String,
    pub content: String,
    pub phase: Option<usize>,
    pub phase_status: PhaseStatus,
}

#[derive(Debug, Clone)]
pub struct PlanDocument {
    pub slug: String,
    pub title: String,
    pub overview: String,
    pub status: PlanStatus,
    pub sections: Vec<PlanSection>,
}

impl PlanDocument {
    /// Phases done / phases total. Sections without a `phase:` tag are not
    /// executable work and are excluded from both numbers, so a plan whose
    /// sections are all prose reports `0/0` rather than a false `0/7`.
    pub fn phase_progress(&self) -> (usize, usize) {
        // Parts of one phase share a phase number; count each phase once and
        // treat it as done only when every part is done.
        let mut phases: Vec<(usize, bool)> = Vec::new();
        for s in self.sections.iter().filter(|s| s.phase.is_some()) {
            let phase = s.phase.unwrap();
            let done = s.phase_status == PhaseStatus::Done;
            match phases.iter_mut().find(|(p, _)| *p == phase) {
                Some(entry) => entry.1 = entry.1 && done,
                None => phases.push((phase, done)),
            }
        }
        (
            phases.iter().filter(|(_, done)| *done).count(),
            phases.len(),
        )
    }

    /// Reassemble the whole document as markdown, in section order.
    pub fn render(&self) -> String {
        let (done, total) = self.phase_progress();
        let mut out = format!("# {}\n\n", self.title);
        out.push_str(&format!("_status: {}", self.status));
        if total > 0 {
            out.push_str(&format!(" · phases: {done}/{total}"));
        }
        out.push_str("_\n\n");
        if !self.overview.trim().is_empty() {
            out.push_str(self.overview.trim());
            out.push_str("\n\n");
        }
        for section in &self.sections {
            out.push_str(&format!("## {}", section.title));
            if let Some(phase) = section.phase {
                out.push_str(&format!(" _(phase {phase} — {})_", section.phase_status));
            }
            out.push_str("\n\n");
            out.push_str(section.content.trim());
            out.push_str("\n\n");
        }
        out
    }
}

#[derive(Debug, Clone)]
pub struct PlanSummary {
    pub slug: String,
    pub title: String,
    pub status: PlanStatus,
    pub phases_done: usize,
    pub phases_total: usize,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone)]
pub struct IceboxItem {
    pub slug: String,
    pub title: String,
    pub content: String,
    pub status: IceboxStatus,
    pub promoted_to: Option<String>,
    pub tags: Vec<String>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

// ── Section input and chunking ──────────────────────────────────────

#[derive(Debug, Clone)]
pub struct SectionInput {
    pub title: String,
    pub content: String,
    /// Set when this section is an executable phase rather than prose.
    pub phase: Option<usize>,
    /// `None` keeps the phase's stored status when the plan is re-created
    /// (see `PlanStore::create_plan`); a new phase starts as pending.
    pub phase_status: Option<PhaseStatus>,
}

impl SectionInput {
    pub fn prose(title: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            content: content.into(),
            phase: None,
            phase_status: None,
        }
    }

    pub fn phase(phase: usize, title: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            content: content.into(),
            phase: Some(phase),
            phase_status: None,
        }
    }
}

/// Appended to every part of a split section but the last.
const SPLIT_MARKER: &str = "\n\n… [continues in the next part]";

/// Split one section's body into as many parts as the content cap requires.
///
/// `propose` would otherwise truncate it, and a truncated *structured*
/// document is the worst outcome available: it parses, it just quietly says
/// less than it claims — the same shape as the crawl-state blob that
/// deserialized to `None` and silently downgraded every later crawl. Parts
/// are split on a paragraph boundary where one exists within the budget, so
/// a split lands between thoughts rather than mid-sentence.
pub fn split_section(content: &str, limit: usize) -> Vec<String> {
    if content.chars().count() <= limit {
        return vec![content.to_string()];
    }
    // Leave room for the continuation marker each non-final part carries.
    let marker = SPLIT_MARKER;
    let budget = limit.saturating_sub(marker.chars().count() + 1).max(1);

    let mut parts = Vec::new();
    let mut rest = content;
    while rest.chars().count() > limit {
        let head = crate::text::char_prefix(rest, budget);
        // Prefer the last paragraph break, then the last line break, then
        // the raw budget. Measured in chars throughout: this repo has
        // broken a hook by comparing a byte length against a char budget.
        let cut = head
            .rfind("\n\n")
            .or_else(|| head.rfind('\n'))
            .filter(|idx| head[..*idx].chars().count() > budget / 4)
            .unwrap_or(head.len());
        let (chunk, _) = head.split_at(cut);
        parts.push(format!("{}{marker}", chunk.trim_end()));
        rest = &rest[chunk.len()..];
        rest = rest.trim_start_matches('\n');
    }
    if !rest.trim().is_empty() {
        parts.push(rest.to_string());
    }
    parts
}

/// A plan parsed out of a markdown document.
#[derive(Debug, Clone)]
pub struct ParsedMarkdown {
    pub title: Option<String>,
    pub overview: String,
    pub sections: Vec<SectionInput>,
    /// Headings that name a phase but sit at a level other than `##`, so
    /// they were folded into their parent section and are **not** tracked.
    ///
    /// Nesting phases under a `## Phases` umbrella is the most natural way
    /// to write this document and produces zero tracked phases in silence
    /// — which is the failure this whole feature exists to avoid. The
    /// callers surface these as a warning rather than guessing, because
    /// promoting them automatically would make an `### Phase 2` inside a
    /// worked example into executable work.
    pub untracked_phase_headings: Vec<String>,
}

/// Parse a plan written as markdown: a leading `# Title`, prose before the
/// first `## Heading` as the overview, and one section per `##`.
///
/// A heading naming a phase (`## Phase 2 — storage`, `## Phase 2: storage`)
/// becomes an executable phase; every other section is prose. That rule is
/// deliberately syntactic rather than clever: a plan author can see from the
/// heading alone whether a section will be tracked, and no section is
/// silently promoted into work.
pub fn parse_markdown(text: &str) -> ParsedMarkdown {
    // Strip the whole document before splitting it: a block that spans a
    // heading would otherwise be cut in two, the heading inside it would
    // become a section (title and key), and the half after the cut has no
    // opening tag left for propose to find.
    let (text, _) = crate::redact::strip_private(text);
    let mut title = None;
    let mut overview = String::new();
    let mut sections: Vec<SectionInput> = Vec::new();
    let mut current: Option<(String, String)> = None;
    let mut untracked_phase_headings = Vec::new();

    for line in text.lines() {
        // Any deeper heading naming a phase is body text by the rule below,
        // but silently so — record it for the caller to warn about.
        if let Some(rest) = line.strip_prefix("### ") {
            if parse_phase_number(rest).is_some() {
                untracked_phase_headings.push(rest.trim().to_string());
            }
        }
        if let Some(rest) = line.strip_prefix("## ") {
            if let Some((heading, body)) = current.take() {
                sections.push(section_from_heading(&heading, &body));
            }
            current = Some((rest.trim().to_string(), String::new()));
        } else if let Some(rest) = line.strip_prefix("# ") {
            if title.is_none() && current.is_none() {
                title = Some(rest.trim().to_string());
                continue;
            }
            // A `#` after the first section is body text, not a title.
            if let Some((_, body)) = current.as_mut() {
                body.push_str(line);
                body.push('\n');
            }
        } else if let Some((_, body)) = current.as_mut() {
            body.push_str(line);
            body.push('\n');
        } else {
            overview.push_str(line);
            overview.push('\n');
        }
    }
    if let Some((heading, body)) = current {
        sections.push(section_from_heading(&heading, &body));
    }

    ParsedMarkdown {
        title,
        overview: overview.trim().to_string(),
        sections,
        untracked_phase_headings,
    }
}

fn section_from_heading(heading: &str, body: &str) -> SectionInput {
    let body = body.trim().to_string();
    match parse_phase_number(heading) {
        Some(n) => SectionInput::phase(n, heading, body),
        None => SectionInput::prose(heading, body),
    }
}

/// Pull `N` out of a heading naming a phase. Case-insensitive; tolerates
/// the separators these documents actually use (`—`, `-`, `:`, `.`).
fn parse_phase_number(heading: &str) -> Option<usize> {
    let lower = heading.to_lowercase();
    let idx = lower.find("phase")?;
    lower[idx + "phase".len()..]
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .ok()
}

/// The key for part `part` (1-based) of `parts` total. A single-part section
/// keeps the bare section key, so the common case carries no suffix noise.
pub fn part_key(section_key: &str, part: usize, parts: usize) -> String {
    if parts <= 1 {
        section_key.to_string()
    } else {
        format!("{section_key}{PART_SEP}{part:02}")
    }
}

/// The heading slug inside a section key: `<name>` in
/// `plan:<slug>:<nn>-<name>[--pNN]`. `slugify` never emits two dashes in a
/// row, so the part suffix cannot be confused with the name.
fn section_name(key: &str) -> Option<&str> {
    let (_, section) = parse_plan_key(key)?;
    let (_, name) = section?.split_once('-')?;
    name.split(PART_SEP).next()
}

/// One stored phase part, as the carry rule sees it.
struct StoredPhase<'a> {
    name: &'a str,
    phase: usize,
    status: PhaseStatus,
    content: &'a str,
    key: &'a str,
}

fn stored_phases(entries: &[MemoryEntry]) -> Vec<StoredPhase<'_>> {
    entries
        .iter()
        .filter_map(|e| {
            let key = e.topic_key.as_deref()?;
            Some(StoredPhase {
                name: section_name(key)?,
                phase: tag_value(&e.tags, PHASE_TAG)?.parse().ok()?,
                status: tag_value(&e.tags, PHASE_STATUS_TAG)
                    .and_then(PhaseStatus::parse)
                    .unwrap_or_default(),
                content: &e.content,
                key,
            })
        })
        .collect()
}

/// What a phase section inherits when its plan is written again.
#[derive(Debug, PartialEq)]
enum Carry {
    /// The stored progress applies; `changed` when the body differs.
    Kept { status: PhaseStatus, changed: bool },
    /// The heading matches no stored phase, but stored progress existed
    /// for its phase number: it starts over, and the caller says so.
    Restarted,
    /// Nothing stored to inherit.
    New,
}

/// The carry rule, shared by `create_plan` and `save_section`, in order:
///
/// 1. the stored phase with the same heading slug, when every part of it
///    carries one phase number (a renumbered phase keeps its progress);
/// 2. otherwise the stored phase with the same number, when one heading
///    slug extends the other at a `-` — `Phase 1 — x — SHIPPED` is still
///    `Phase 1 — x`;
/// 3. otherwise nothing: the phase starts as pending.
///
/// Parts of one phase carry the least advanced of their statuses, so a
/// half-finished phase never reads as done. Redo, never skip: a renamed
/// heading restarts.
fn carry_phase(stored: &[StoredPhase<'_>], name: &str, phase: usize, content: &str) -> Carry {
    let extends = |longer: &str, shorter: &str| {
        longer
            .strip_prefix(shorter)
            .is_some_and(|rest| rest.starts_with('-'))
    };
    let same_heading: Vec<&StoredPhase<'_>> = stored.iter().filter(|p| p.name == name).collect();
    let matched: Vec<&StoredPhase<'_>> = if !same_heading.is_empty()
        && same_heading
            .iter()
            .all(|p| p.phase == same_heading[0].phase)
    {
        same_heading
    } else {
        stored
            .iter()
            .filter(|p| p.phase == phase && (extends(p.name, name) || extends(name, p.name)))
            .collect()
    };
    let Some(status) = matched
        .iter()
        .map(|p| p.status)
        .min_by_key(|s| phase_rank(*s))
    else {
        return if stored
            .iter()
            .any(|p| p.phase == phase && p.status != PhaseStatus::Pending)
        {
            Carry::Restarted
        } else {
            Carry::New
        };
    };
    // Compare bodies the way the duplicate guard does (case and whitespace
    // folded), over the parts in key order with the split markers removed.
    let mut parts = matched;
    parts.sort_by_key(|p| p.key);
    let stored_body: String = parts
        .iter()
        .map(|p| p.content.replace(SPLIT_MARKER, ""))
        .collect::<Vec<_>>()
        .join("\n");
    let changed =
        crate::storage::content_hash("", &stored_body) != crate::storage::content_hash("", content);
    Carry::Kept { status, changed }
}

// ── Reading ─────────────────────────────────────────────────────────

fn section_from_entry(entry: &MemoryEntry) -> Option<PlanSection> {
    let topic_key = entry.topic_key.clone()?;
    let (_, section) = parse_plan_key(&topic_key)?;
    let section = section?;
    let index = section
        .split_once('-')
        .and_then(|(n, _)| n.parse::<usize>().ok())?;
    Some(PlanSection {
        topic_key,
        index,
        title: entry.title.clone(),
        content: entry.content.clone(),
        phase: tag_value(&entry.tags, PHASE_TAG).and_then(|v| v.parse().ok()),
        phase_status: tag_value(&entry.tags, PHASE_STATUS_TAG)
            .and_then(PhaseStatus::parse)
            .unwrap_or_default(),
    })
}

/// Does this key belong to `slug` — its meta entry or one of its sections?
///
/// Both halves matter. Without the exact-match arm a caller handed every
/// `plan:` key merges one plan's sections into another; without the `:`
/// on the section arm, `plan:auth` claims `plan:auth-flow:00-intro`,
/// because one slug is a prefix of the other.
fn key_belongs_to(slug: &str, key: &str) -> bool {
    let meta = plan_meta_key(slug);
    key == meta || key.starts_with(&format!("{meta}:"))
}

/// Is this entry part of plan `slug` — the key boundary *and* the type?
///
/// The key half stops `plan:auth` claiming `plan:auth-flow`; the type half
/// stops a decision saved under a `plan:` key (the shape the old protocol
/// advice produced) reading as a plan, or being retired as one.
fn owned_by_plan(slug: &str, entry: &MemoryEntry) -> bool {
    entry.entry_type == EntryType::Plan
        && entry
            .topic_key
            .as_deref()
            .is_some_and(|k| key_belongs_to(slug, k))
}

fn plan_from_entries(slug: &str, entries: &[MemoryEntry]) -> Option<PlanDocument> {
    let meta_key = plan_meta_key(slug);
    // Two live metas can exist when another machine's write crossed this
    // one; the newest is the plan's current state.
    let meta = entries
        .iter()
        .filter(|e| {
            e.entry_type == EntryType::Plan && e.topic_key.as_deref() == Some(meta_key.as_str())
        })
        .max_by_key(|e| e.created_at)?;
    // Scope here rather than at the call sites: `list_plans` passes every
    // `plan:` entry in the namespace, and merging their phases together
    // made each plan report the union of all of them.
    let mut sections: Vec<PlanSection> = entries
        .iter()
        .filter(|e| owned_by_plan(slug, e))
        .filter_map(section_from_entry)
        .collect();
    // The storage query orders by topic_key, which is already reading order;
    // sort again so a caller that assembled the slice by hand is safe too.
    sections.sort_by(|a, b| a.topic_key.cmp(&b.topic_key));
    Some(PlanDocument {
        slug: slug.to_string(),
        title: meta.title.clone(),
        overview: meta.content.clone(),
        status: tag_value(&meta.tags, STATUS_TAG)
            .and_then(PlanStatus::parse)
            .unwrap_or_default(),
        sections,
    })
}

fn icebox_from_entry(entry: &MemoryEntry) -> Option<IceboxItem> {
    if entry.entry_type != EntryType::Icebox {
        return None;
    }
    let topic_key = entry.topic_key.as_deref()?;
    let slug = parse_icebox_key(topic_key)?;
    Some(IceboxItem {
        slug: slug.to_string(),
        title: entry.title.clone(),
        content: entry.content.clone(),
        status: tag_value(&entry.tags, STATUS_TAG)
            .and_then(IceboxStatus::parse)
            .unwrap_or_default(),
        promoted_to: tag_value(&entry.tags, PROMOTED_TAG).map(|s| s.to_string()),
        tags: entry.tags.clone(),
        updated_at: entry.updated_at,
    })
}

/// The slug for a new plan or icebox item. A slug derived from a title
/// that holds a private block would carry its words into the key, which is
/// stored as given; such a title needs an explicit slug. An explicit slug
/// may not hold a block either.
fn slug_for(title: &str, slug: Option<&str>) -> StorageResult<String> {
    use crate::storage::StorageError::Invalid;
    match slug {
        Some(explicit) => {
            if crate::redact::strip_private(explicit).1 > 0 {
                return Err(Invalid("a slug cannot hold a private block".to_string()));
            }
            Ok(slugify(explicit))
        }
        None if crate::redact::strip_private(title).1 > 0 => Err(Invalid(
            "the title holds a private block, so a slug derived from it would expose it: \
             pass an explicit slug"
                .to_string(),
        )),
        None => Ok(slugify(title)),
    }
}

/// A section with its private blocks removed, before its key is built and
/// before it is split into parts. Stripping per part, after the split,
/// leaves the text between a part boundary and the closing tag in the
/// clear.
fn public_section(section: &SectionInput) -> (SectionInput, bool) {
    let (title, title_blocks) = crate::redact::strip_private(&section.title);
    let (content, content_blocks) = crate::redact::strip_private(&section.content);
    (
        SectionInput {
            title,
            content,
            ..section.clone()
        },
        title_blocks + content_blocks > 0,
    )
}

/// What `create_plan` did, for the caller to report.
#[derive(Debug, Clone)]
pub struct CreatedPlan {
    pub slug: String,
    /// Entries in the plan's key set: the meta plus every section part.
    pub entries: usize,
    /// Phase headings that restarted as pending although their phase
    /// number had progress: the heading changed beyond an annotation.
    pub restarted_phases: Vec<String>,
    /// Phase headings that kept `done` although their body changed.
    pub done_phases_changed: Vec<String>,
}

impl CreatedPlan {
    /// One sentence per notice, for the MCP `warning` and the CLI.
    pub fn notices(&self) -> Vec<String> {
        let mut out = Vec::new();
        if !self.restarted_phases.is_empty() {
            out.push(format!(
                "{} phase(s) restarted as pending because the heading changed: {}. Set \
                 them back with muninn_plan_set_status if the work is already done.",
                self.restarted_phases.len(),
                self.restarted_phases.join("; ")
            ));
        }
        if !self.done_phases_changed.is_empty() {
            out.push(format!(
                "{} phase(s) are still done although their body changed: {}. Reset them \
                 with muninn_plan_set_status if the change is new work.",
                self.done_phases_changed.len(),
                self.done_phases_changed.join("; ")
            ));
        }
        out
    }
}

/// One entry a write means to leave in the store.
struct PlannedEntry {
    key: String,
    title: String,
    content: String,
    tags: Vec<String>,
}

impl PlannedEntry {
    /// Would propose store exactly what `stored` holds? Title and content
    /// go through the same secret redaction and bound first.
    fn matches(&self, stored: &MemoryEntry) -> bool {
        let (title, _) = crate::redact::redact_secrets(&self.title);
        let (content, _) = crate::redact::redact_secrets(&self.content);
        let (content, _) = crate::librarian::bound_content(&content, content_limit());
        crate::storage::content_hash(&title, &content)
            == crate::storage::content_hash(&stored.title, &stored.content)
    }
}

// ── Store: the operations both surfaces call ────────────────────────

/// Thin façade over `MemoryLibrarian` holding the plan/icebox conventions.
/// Every write goes through `propose`, so redaction, content bounding, the
/// supersede edge and the outbox enqueue all apply exactly as they do to any
/// other entry.
pub struct PlanStore<'a> {
    lib: &'a MemoryLibrarian,
    project_id: Option<String>,
}

impl<'a> PlanStore<'a> {
    pub fn new(lib: &'a MemoryLibrarian, project_id: Option<String>) -> Self {
        Self { lib, project_id }
    }

    async fn entries_under(&self, prefix: &str) -> StorageResult<Vec<MemoryEntry>> {
        self.lib
            .list_by_topic_prefix(self.project_id.as_deref(), prefix)
            .await
    }

    /// Every live entry of plan `slug` and nothing else: the prefix query
    /// filtered by `owned_by_plan`.
    async fn plan_entries(&self, slug: &str) -> StorageResult<Vec<MemoryEntry>> {
        Ok(self
            .entries_under(&plan_scope_prefix(slug))
            .await?
            .into_iter()
            .filter(|e| owned_by_plan(slug, e))
            .collect())
    }

    /// Find plan `slug` the way it was stored: the exact slug first, so a
    /// legacy slug written before every writer slugified still resolves,
    /// then its slugified form, which is what `create_plan` writes — so
    /// `Auth`, `AUTH` and `auth flow` reach the plans `auth` and
    /// `auth-flow`. Returns the resolved slug and the plan's own entries.
    ///
    /// Resolution never relies on the storage `LIKE`: it is case-insensitive
    /// for ASCII on SQLite and case-sensitive on Postgres. The slug is
    /// normalized before the query and the final match is an exact Rust
    /// comparison, so both backends agree.
    async fn resolve_plan(&self, slug: &str) -> StorageResult<Option<(String, Vec<MemoryEntry>)>> {
        let normalized = slugify(slug);
        let candidates = if normalized == slug {
            vec![normalized]
        } else {
            vec![slug.to_string(), normalized]
        };
        for candidate in candidates {
            let entries = self.plan_entries(&candidate).await?;
            let meta_key = plan_meta_key(&candidate);
            if entries
                .iter()
                .any(|e| e.topic_key.as_deref() == Some(meta_key.as_str()))
            {
                return Ok(Some((candidate, entries)));
            }
        }
        Ok(None)
    }

    /// The live icebox entry for `slug`, typed as one, resolved exact-first
    /// like `resolve_plan`. The type check sits in the predicate, so a note
    /// at the same key never hides the item.
    async fn icebox_entry(&self, slug: &str) -> StorageResult<Option<MemoryEntry>> {
        let normalized = slugify(slug);
        let candidates = if normalized == slug {
            vec![normalized]
        } else {
            vec![slug.to_string(), normalized]
        };
        for candidate in candidates {
            let key = icebox_key(&candidate);
            let found = self.entries_under(&key).await?.into_iter().find(|e| {
                e.entry_type == EntryType::Icebox && e.topic_key.as_deref() == Some(key.as_str())
            });
            if found.is_some() {
                return Ok(found);
            }
        }
        Ok(None)
    }

    /// `propose` with the store's key and project, nothing more. Only
    /// `save_section` still writes this way.
    async fn save(
        &self,
        title: &str,
        content: &str,
        entry_type: EntryType,
        topic_key: &str,
        tags: Vec<String>,
    ) -> StorageResult<()> {
        self.lib
            .propose(MemoryEntryInput {
                title: title.to_string(),
                content: content.to_string(),
                entry_type,
                tags,
                project_id: self.project_id.clone(),
                topic_key: Some(topic_key.to_string()),
                ..Default::default()
            })
            .await?;
        Ok(())
    }

    /// Write one entry so that afterwards the store holds exactly it.
    ///
    /// Two things `propose` alone does not do. An unchanged entry comes
    /// back as a duplicate and its tags are never written, so a new status
    /// on unchanged text was dropped: the store's own tags are replaced
    /// here, keeping every other tag. And a topic-key supersede soft-deletes
    /// the predecessor without queuing a delete, so the remote kept it
    /// live — two metas for one plan; the predecessor is deprecated here,
    /// which queues the tombstone.
    async fn put(&self, entry: &PlannedEntry, entry_type: EntryType) -> StorageResult<()> {
        let result = self
            .lib
            .propose(MemoryEntryInput {
                title: entry.title.clone(),
                content: entry.content.clone(),
                entry_type,
                tags: entry.tags.clone(),
                project_id: self.project_id.clone(),
                topic_key: Some(entry.key.clone()),
                ..Default::default()
            })
            .await?;
        if result.action == SaveAction::Duplicate {
            let stored = self.lib.get(result.id).await?;
            let mut merged: Vec<String> = stored
                .tags
                .iter()
                .filter(|t| !is_store_tag(t))
                .cloned()
                .collect();
            for tag in &entry.tags {
                if !merged.contains(tag) {
                    merged.push(tag.clone());
                }
            }
            let mut want = merged.clone();
            let mut have = stored.tags.clone();
            want.sort();
            have.sort();
            if want != have {
                self.lib.retag(result.id, merged).await?;
            }
        }
        if let Some(old) = result.superseded {
            self.lib.deprecate(old.id).await?;
        }
        Ok(())
    }

    // ── Plans ───────────────────────────────────────────────────

    /// Create or replace a plan: meta entry plus one entry per section part.
    ///
    /// Re-creating keeps progress. Without `status` the stored plan status
    /// stays (a new plan starts as draft), and every phase whose heading
    /// is unchanged — or only annotated — keeps its status by the carry
    /// rule (`carry_phase`); the result reports phases that restarted and
    /// done phases whose body changed. A completed or abandoned plan is not
    /// reopened by accident: changing it without a `status` is refused
    /// before anything is written.
    pub async fn create_plan(
        &self,
        title: &str,
        slug: Option<&str>,
        overview: &str,
        sections: &[SectionInput],
        status: Option<PlanStatus>,
    ) -> StorageResult<CreatedPlan> {
        let slug = slug_for(title, slug)?;
        let limit = content_limit();
        let existing = self.plan_entries(&slug).await?;
        let meta_key = plan_meta_key(&slug);
        let stored_status = plan_from_entries(&slug, &existing).map(|d| d.status);
        let stored = stored_phases(&existing);

        let mut planned = Vec::new();
        let mut meta_tags = vec!["plan".to_string()];
        let meta_status = status.or(stored_status).unwrap_or_default();
        set_tag(&mut meta_tags, STATUS_TAG, meta_status.as_str());
        planned.push(PlannedEntry {
            key: meta_key,
            title: title.to_string(),
            content: overview.to_string(),
            tags: meta_tags,
        });

        let mut created = CreatedPlan {
            slug: slug.clone(),
            entries: 0,
            restarted_phases: Vec::new(),
            done_phases_changed: Vec::new(),
        };
        for (idx, section) in sections.iter().enumerate() {
            let (section, redacted) = public_section(section);
            let phase_status = section.phase.map(|phase| {
                section.phase_status.unwrap_or_else(|| {
                    let (body, _) = crate::redact::redact_secrets(&section.content);
                    match carry_phase(&stored, &slugify(&section.title), phase, &body) {
                        Carry::Kept { status, changed } => {
                            if changed && status == PhaseStatus::Done {
                                created.done_phases_changed.push(section.title.clone());
                            }
                            status
                        }
                        Carry::Restarted => {
                            created.restarted_phases.push(section.title.clone());
                            PhaseStatus::Pending
                        }
                        Carry::New => PhaseStatus::Pending,
                    }
                })
            });
            let section_key = plan_section_key(&slug, idx, &section.title);
            let parts = split_section(&section.content, limit);
            for (part_idx, body) in parts.iter().enumerate() {
                let mut tags = vec!["plan".to_string(), format!("{PLAN_REF_TAG}{slug}")];
                if redacted {
                    tags.push("redacted".to_string());
                }
                if let (Some(phase), Some(phase_status)) = (section.phase, phase_status) {
                    set_tag(&mut tags, PHASE_TAG, &phase.to_string());
                    set_tag(&mut tags, PHASE_STATUS_TAG, phase_status.as_str());
                }
                planned.push(PlannedEntry {
                    key: part_key(&section_key, part_idx + 1, parts.len()),
                    title: if parts.len() > 1 {
                        format!("{} (part {}/{})", section.title, part_idx + 1, parts.len())
                    } else {
                        section.title.clone()
                    },
                    content: body.clone(),
                    tags,
                });
            }
        }

        if status.is_none() && stored_status.is_some_and(|s| s.is_closed()) {
            let unchanged = planned.len() == existing.len()
                && planned.iter().all(|p| {
                    existing
                        .iter()
                        .any(|e| e.topic_key.as_deref() == Some(p.key.as_str()) && p.matches(e))
                });
            if !unchanged {
                let closed = stored_status.unwrap_or_default();
                return Err(crate::storage::StorageError::Invalid(format!(
                    "plan '{slug}' is {closed}; pass status to reopen it, or use another slug"
                )));
            }
        }

        for entry in &planned {
            self.put(entry, EntryType::Plan).await?;
        }

        // Retire anything of this plan's that this write did not produce — a
        // section removed from the plan, or a trailing part of a section
        // that shrank. The completeness premise holds over the plan's own
        // keys and only there: we just wrote the plan's entire key set, so
        // an owned key absent from it is stale by construction. The raw
        // prefix query does not return the plan's keys alone — it also
        // returns every longer slug's and any decision filed under the
        // scope — which is why the candidates go through `plan_entries`.
        // (The retired `.sql` reconciliation pass failed because its
        // premise did not hold either.)
        let stale: Vec<_> = self
            .plan_entries(&slug)
            .await?
            .into_iter()
            .filter(|e| {
                e.topic_key
                    .as_deref()
                    .is_some_and(|k| !planned.iter().any(|p| p.key == k))
            })
            .collect();
        for entry in &stale {
            self.lib.deprecate(entry.id).await?;
        }

        created.entries = planned.len();
        Ok(created)
    }

    /// Upsert one section of an existing plan, leaving the rest alone.
    pub async fn save_section(
        &self,
        slug: &str,
        index: usize,
        title: &str,
        content: &str,
        phase: Option<usize>,
    ) -> StorageResult<String> {
        let (title, title_blocks) = crate::redact::strip_private(title);
        let (content, content_blocks) = crate::redact::strip_private(content);
        let (title, content) = (title.as_str(), content.as_str());
        let section_key = plan_section_key(slug, index, title);
        let parts = split_section(content, content_limit());
        let mut written = Vec::new();
        for (part_idx, body) in parts.iter().enumerate() {
            let key = part_key(&section_key, part_idx + 1, parts.len());
            let mut tags = vec!["plan".to_string(), format!("{PLAN_REF_TAG}{slug}")];
            if title_blocks + content_blocks > 0 {
                tags.push("redacted".to_string());
            }
            if let Some(phase) = phase {
                set_tag(&mut tags, PHASE_TAG, &phase.to_string());
                set_tag(&mut tags, PHASE_STATUS_TAG, PhaseStatus::Pending.as_str());
            }
            let part_title = if parts.len() > 1 {
                format!("{title} (part {}/{})", part_idx + 1, parts.len())
            } else {
                title.to_string()
            };
            self.save(&part_title, body, EntryType::Plan, &key, tags)
                .await?;
            written.push(key);
        }
        // Same bounded reconciliation as `create_plan`, scoped to this one
        // section: drop parts left over from a longer previous version.
        for entry in self.entries_under(&section_key).await? {
            if entry
                .topic_key
                .as_deref()
                .is_some_and(|k| !written.iter().any(|w| w == k))
            {
                self.lib.deprecate(entry.id).await?;
            }
        }
        Ok(section_key)
    }

    pub async fn get_plan(&self, slug: &str) -> StorageResult<Option<PlanDocument>> {
        Ok(self
            .resolve_plan(slug)
            .await?
            .and_then(|(slug, entries)| plan_from_entries(&slug, &entries)))
    }

    pub async fn list_plans(
        &self,
        status_filter: Option<PlanStatus>,
    ) -> StorageResult<Vec<PlanSummary>> {
        let entries = self.entries_under(PLAN_PREFIX).await?;
        let mut summaries = Vec::new();
        for entry in &entries {
            if entry.entry_type != EntryType::Plan {
                continue;
            }
            let Some(key) = entry.topic_key.as_deref() else {
                continue;
            };
            // Meta entries only: `plan:<slug>` with no section remainder.
            let Some((slug, None)) = parse_plan_key(key) else {
                continue;
            };
            let Some(doc) = plan_from_entries(slug, &entries) else {
                continue;
            };
            if status_filter.is_some_and(|want| want != doc.status) {
                continue;
            }
            let (done, total) = doc.phase_progress();
            summaries.push(PlanSummary {
                slug: slug.to_string(),
                title: doc.title,
                status: doc.status,
                phases_done: done,
                phases_total: total,
                updated_at: entry.updated_at,
            });
        }
        summaries.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
        Ok(summaries)
    }

    /// Set a plan's status, or one phase's status when `phase` is given.
    ///
    /// Retags the affected entries in place (same id), so the transition
    /// enqueues to the outbox like any other write. Everything is checked
    /// before the first write: an unknown slug or a phase the plan does not
    /// have changes nothing, and only the named plan's entries are touched
    /// — never a sibling that shares the slug as a prefix.
    pub async fn set_plan_status(
        &self,
        slug: &str,
        status: Option<PlanStatus>,
        phase: Option<usize>,
        phase_status: Option<PhaseStatus>,
    ) -> StorageResult<PlanDocument> {
        use crate::storage::StorageError::Invalid;

        let Some((slug, entries)) = self.resolve_plan(slug).await? else {
            return Err(Invalid(format!("no plan with slug '{slug}'")));
        };
        let meta_key = plan_meta_key(&slug);

        let phase_update = match (phase, phase_status) {
            (Some(phase), Some(phase_status)) => {
                let targets: Vec<&MemoryEntry> = entries
                    .iter()
                    .filter(|e| {
                        tag_value(&e.tags, PHASE_TAG).and_then(|v| v.parse::<usize>().ok())
                            == Some(phase)
                    })
                    .collect();
                if targets.is_empty() {
                    return Err(Invalid(format!("plan '{slug}' has no phase {phase}")));
                }
                Some((targets, phase_status))
            }
            _ => None,
        };

        if let Some((targets, phase_status)) = phase_update {
            for entry in targets {
                let mut tags = entry.tags.clone();
                set_tag(&mut tags, PHASE_STATUS_TAG, phase_status.as_str());
                self.lib.retag(entry.id, tags).await?;
            }
        }
        if let Some(status) = status {
            // Present by construction: resolve_plan only returns a slug
            // whose meta it found.
            if let Some(meta) = entries
                .iter()
                .find(|e| e.topic_key.as_deref() == Some(meta_key.as_str()))
            {
                let mut tags = meta.tags.clone();
                set_tag(&mut tags, STATUS_TAG, status.as_str());
                self.lib.retag(meta.id, tags).await?;
            }
        }

        self.get_plan(&slug).await?.ok_or_else(|| {
            Invalid(format!(
                "plan '{slug}' changed during the update; read it again"
            ))
        })
    }

    // ── Icebox ──────────────────────────────────────────────────

    pub async fn add_icebox(
        &self,
        title: &str,
        content: &str,
        slug: Option<&str>,
        extra_tags: &[String],
    ) -> StorageResult<String> {
        let slug = slug_for(title, slug)?;
        let mut tags = vec!["icebox".to_string()];
        tags.extend(extra_tags.iter().cloned());
        set_tag(&mut tags, STATUS_TAG, IceboxStatus::Open.as_str());
        let entry = PlannedEntry {
            key: icebox_key(&slug),
            title: title.to_string(),
            content: content.to_string(),
            tags,
        };
        self.put(&entry, EntryType::Icebox).await?;
        Ok(slug)
    }

    pub async fn get_icebox(&self, slug: &str) -> StorageResult<Option<IceboxItem>> {
        Ok(self
            .icebox_entry(slug)
            .await?
            .as_ref()
            .and_then(icebox_from_entry))
    }

    pub async fn list_icebox(
        &self,
        status_filter: Option<IceboxStatus>,
    ) -> StorageResult<Vec<IceboxItem>> {
        let mut items: Vec<IceboxItem> = self
            .entries_under(ICEBOX_PREFIX)
            .await?
            .iter()
            .filter_map(icebox_from_entry)
            .filter(|i| status_filter.is_none_or(|want| want == i.status))
            .collect();
        items.sort_by_key(|i| std::cmp::Reverse(i.updated_at));
        Ok(items)
    }

    pub async fn set_icebox_status(
        &self,
        slug: &str,
        status: IceboxStatus,
        promoted_to: Option<&str>,
    ) -> StorageResult<IceboxItem> {
        let entry = self.icebox_entry(slug).await?.ok_or_else(|| {
            crate::storage::StorageError::Invalid(format!("no icebox item with slug '{slug}'"))
        })?;

        let mut tags = entry.tags.clone();
        set_tag(&mut tags, STATUS_TAG, status.as_str());
        if let Some(plan_slug) = promoted_to {
            set_tag(&mut tags, PROMOTED_TAG, plan_slug);
        }
        self.lib.retag(entry.id, tags).await?;

        self.get_icebox(slug).await?.ok_or_else(|| {
            crate::storage::StorageError::Invalid(format!(
                "icebox item '{slug}' changed during the update; read it again"
            ))
        })
    }
}

/// One scenario, run over in-memory SQLite (`tests` below) and live
/// Postgres (`storage::postgres` tests): the two backends have diverged on
/// plan identity before, so both must keep every plan's data the same way.
#[cfg(test)]
pub(crate) mod parity {
    use super::*;

    /// Slugs that collide in every way the store has got wrong: a prefix
    /// pair, a case variant and a spaced variant.
    pub(crate) const COLLIDING_SLUGS: [&str; 4] = ["auth", "auth-flow", "AUTH", "auth flow"];

    async fn propose_other(
        lib: &MemoryLibrarian,
        project: &str,
        t: EntryType,
        key: &str,
    ) -> uuid::Uuid {
        lib.propose(MemoryEntryInput {
            title: format!("{t:?} under {key}"),
            content: format!("filed under {key} by a generic save"),
            entry_type: t,
            project_id: Some(project.to_string()),
            topic_key: Some(key.to_string()),
            ..Default::default()
        })
        .await
        .unwrap()
        .id
    }

    pub(crate) async fn keeps_every_plans_data(lib: &MemoryLibrarian, project: &str) {
        let [auth, auth_flow, upper, _] = COLLIDING_SLUGS;
        let store = PlanStore::new(lib, Some(project.to_string()));

        store
            .create_plan(
                "auth flow",
                Some(auth_flow),
                "the longer slug",
                &[
                    SectionInput::phase(1, "Phase 1 — tokens", "issue tokens"),
                    SectionInput::prose("Notes", "auth-flow's own notes"),
                ],
                Some(PlanStatus::Draft),
            )
            .await
            .unwrap();
        let ghost = propose_other(lib, project, EntryType::Decision, "plan:ghost").await;
        let note = propose_other(lib, project, EntryType::Note, "icebox:x").await;
        store
            .create_plan(
                "auth",
                Some(auth),
                "the prefix slug",
                &[SectionInput::phase(1, "Phase 1 — login", "log in")],
                Some(PlanStatus::Draft),
            )
            .await
            .unwrap();

        let flow = store
            .get_plan(auth_flow)
            .await
            .unwrap()
            .expect("auth-flow survives auth");
        assert_eq!(
            flow.sections
                .iter()
                .map(|s| s.title.as_str())
                .collect::<Vec<_>>(),
            ["Phase 1 — tokens", "Notes"]
        );
        let slugs: Vec<String> = store
            .list_plans(None)
            .await
            .unwrap()
            .into_iter()
            .map(|p| p.slug)
            .collect();
        assert_eq!(
            slugs.len(),
            2,
            "a decision under plan: is not a plan: {slugs:?}"
        );
        assert!(store.get_plan("ghost").await.unwrap().is_none());
        assert!(store.get_icebox("x").await.unwrap().is_none());
        lib.get(ghost)
            .await
            .expect("the decision under plan: is not retired");
        lib.get(note)
            .await
            .expect("the note under icebox: is untouched");

        // A case variant resolves to `auth` and advances only auth.
        let advanced = store
            .set_plan_status(upper, None, Some(1), Some(PhaseStatus::Done))
            .await
            .unwrap();
        assert_eq!(advanced.slug, auth);
        assert_eq!(advanced.phase_progress(), (1, 1));
        let flow = store.get_plan(auth_flow).await.unwrap().unwrap();
        assert_eq!(
            flow.phase_progress(),
            (0, 1),
            "the prefix sibling is untouched"
        );
        assert!(store
            .set_plan_status("missing", Some(PlanStatus::Completed), None, None)
            .await
            .is_err());

        store
            .set_plan_status(auth_flow, None, Some(1), Some(PhaseStatus::Done))
            .await
            .unwrap();
        // Re-create auth-flow with a section inserted first: every later
        // section moves to a new index and must still be there, with its
        // progress.
        store
            .create_plan(
                "auth flow",
                Some(auth_flow),
                "the longer slug",
                &[
                    SectionInput::prose("Context", "why auth-flow exists"),
                    SectionInput::phase(1, "Phase 1 — tokens", "issue tokens"),
                    SectionInput::prose("Notes", "auth-flow's own notes"),
                ],
                Some(PlanStatus::Draft),
            )
            .await
            .unwrap();
        // A third plan holding a section identical to one of auth-flow's.
        store
            .create_plan(
                "other",
                Some("other"),
                "a third plan",
                &[SectionInput::prose("Notes", "auth-flow's own notes")],
                Some(PlanStatus::Draft),
            )
            .await
            .unwrap();

        let flow = store.get_plan(auth_flow).await.unwrap().unwrap();
        assert_eq!(
            flow.sections
                .iter()
                .map(|s| s.title.as_str())
                .collect::<Vec<_>>(),
            ["Context", "Phase 1 — tokens", "Notes"]
        );
        assert_eq!(
            flow.phase_progress(),
            (1, 1),
            "the moved phase is still done"
        );
        assert_eq!(
            store
                .get_plan(auth)
                .await
                .unwrap()
                .unwrap()
                .phase_progress(),
            (1, 1),
            "auth keeps its own progress"
        );
        let other = store.get_plan("other").await.unwrap().unwrap();
        assert_eq!(
            other.sections.len(),
            1,
            "an identical section is still its own"
        );
        assert_eq!(store.list_plans(None).await.unwrap().len(), 3);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedding::DisabledEmbeddingProvider;
    use crate::storage::sqlite::SqliteAdapter;
    use crate::storage::MemoryStorage;
    use std::sync::Arc;

    async fn test_lib() -> MemoryLibrarian {
        let storage = Arc::new(SqliteAdapter::in_memory("test").unwrap());
        storage.initialize().await.unwrap();
        MemoryLibrarian::new(storage, Arc::new(DisabledEmbeddingProvider), "test", None)
    }

    #[test]
    fn slugify_keeps_the_whole_title() {
        // `suggest_topic_key` takes four words; a plan slug must not, or
        // "auth rewrite phase one" and "auth rewrite phase two" collide.
        assert_eq!(slugify("Auth rewrite phase one"), "auth-rewrite-phase-one");
        assert_eq!(
            slugify("Store the ICEBOX in runar!"),
            "store-the-icebox-in-runar"
        );
        assert_eq!(slugify("  --  "), "untitled");
        assert!(slugify(&"word ".repeat(40)).chars().count() <= 64);
    }

    #[test]
    fn section_keys_sort_into_reading_order() {
        let mut keys = vec![
            plan_section_key("p", 10, "Later"),
            plan_section_key("p", 2, "Middle"),
            plan_section_key("p", 0, "First"),
        ];
        keys.sort();
        // Zero padding is what makes 2 sort before 10 as text.
        assert_eq!(
            keys,
            vec!["plan:p:00-first", "plan:p:02-middle", "plan:p:10-later"]
        );
    }

    #[test]
    fn part_keys_sort_after_their_section_and_before_the_next() {
        let section = plan_section_key("p", 0, "Intro");
        let next = plan_section_key("p", 1, "Body");
        let mut keys = [
            part_key(&section, 2, 3),
            next.clone(),
            part_key(&section, 1, 3),
            part_key(&section, 3, 3),
        ];
        keys.sort();
        assert_eq!(keys.last().unwrap(), &next);
        assert!(keys[0].ends_with("--p01"));
    }

    #[test]
    fn single_part_section_keeps_the_bare_key() {
        let section = plan_section_key("p", 0, "Intro");
        assert_eq!(part_key(&section, 1, 1), section);
    }

    #[test]
    fn parse_plan_key_separates_meta_from_sections() {
        assert_eq!(parse_plan_key("plan:auth"), Some(("auth", None)));
        assert_eq!(
            parse_plan_key("plan:auth:00-intro"),
            Some(("auth", Some("00-intro")))
        );
        assert_eq!(parse_plan_key("icebox:auth"), None);
    }

    #[test]
    fn markdown_parses_title_overview_and_sections() {
        let md = "\
# Icebox campaign

Takes four items from the backlog.
Sync is the hard requirement.

## Context

Why this is being done.

## Phase 1 — types

Add the entry types.

## Phase 2: storage

Add the prefix query.

## Risks

What could go wrong.
";
        let parsed = parse_markdown(md);
        assert_eq!(parsed.title.as_deref(), Some("Icebox campaign"));
        assert!(parsed.overview.contains("Sync is the hard requirement"));
        assert!(
            !parsed.overview.contains("Why this is being done"),
            "prose after the first heading belongs to that section"
        );
        assert_eq!(
            parsed
                .sections
                .iter()
                .map(|s| (s.title.as_str(), s.phase))
                .collect::<Vec<_>>(),
            vec![
                ("Context", None),
                ("Phase 1 — types", Some(1)),
                ("Phase 2: storage", Some(2)),
                ("Risks", None),
            ],
            "only headings naming a phase become tracked work"
        );
        assert_eq!(parsed.sections[1].content, "Add the entry types.");
    }

    #[test]
    fn phases_nested_below_a_heading_are_reported_rather_than_lost() {
        // Found by dogfooding: this campaign's own plan nested its phases
        // under `## Phases`, produced zero tracked phases, and said
        // nothing. Silently untracked work is exactly the failure the
        // phase mechanism exists to prevent.
        let md = "\
# Campaign

## Phases

### Phase 1 — types

do the types

### Phase 2 — storage

do the storage

## Risks

none
";
        let parsed = parse_markdown(md);
        assert!(
            parsed.sections.iter().all(|s| s.phase.is_none()),
            "a `###` heading stays body text — promoting it would make an \
             `### Phase 2` inside a worked example into executable work"
        );
        assert_eq!(
            parsed.untracked_phase_headings,
            vec![
                "Phase 1 — types".to_string(),
                "Phase 2 — storage".to_string()
            ],
            "…but the author has to be told"
        );
    }

    #[test]
    fn a_correctly_written_plan_warns_about_nothing() {
        let parsed = parse_markdown("# P\n\n## Phase 1 — go\n\nbody\n");
        assert_eq!(parsed.sections[0].phase, Some(1));
        assert!(parsed.untracked_phase_headings.is_empty());
    }

    #[test]
    fn markdown_without_a_title_is_still_parsed() {
        let parsed = parse_markdown("## Only a section\n\nbody\n");
        assert!(parsed.title.is_none());
        assert_eq!(parsed.sections.len(), 1);
        assert_eq!(parsed.sections[0].content, "body");
    }

    #[test]
    fn a_hash_inside_a_section_is_body_not_a_title() {
        // Markdown bodies contain `# comments` in shell blocks; treating
        // one as the document title would silently rename the plan.
        let parsed = parse_markdown("# Real\n\n## S\n\n# not a title\n");
        assert_eq!(parsed.title.as_deref(), Some("Real"));
        assert!(parsed.sections[0].content.contains("# not a title"));
    }

    #[test]
    fn phase_numbers_survive_the_separators_these_documents_use() {
        for heading in [
            "Phase 3 — storage",
            "phase 3: storage",
            "PHASE 3. storage",
            "Phase 3",
        ] {
            assert_eq!(parse_phase_number(heading), Some(3), "{heading}");
        }
        assert_eq!(parse_phase_number("Phased rollout"), None);
        assert_eq!(parse_phase_number("Risks"), None);
    }

    #[test]
    fn split_section_respects_the_cap_and_never_loses_text() {
        let body = (0..200)
            .map(|i| format!("paragraph {i} with some words in it"))
            .collect::<Vec<_>>()
            .join("\n\n");
        let parts = split_section(&body, 500);
        assert!(parts.len() > 1, "a 7k-char body must not fit in 500");
        for part in &parts {
            assert!(
                part.chars().count() <= 500,
                "part of {} chars exceeds the cap",
                part.chars().count()
            );
        }
        // Every original paragraph must appear somewhere: a chunker that
        // drops text is the failure this exists to prevent.
        let joined = parts.join("\n");
        for i in [0, 77, 199] {
            assert!(
                joined.contains(&format!("paragraph {i} ")),
                "paragraph {i} was dropped"
            );
        }
    }

    #[test]
    fn split_section_leaves_short_content_untouched() {
        assert_eq!(split_section("short body", 500), vec!["short body"]);
    }

    #[test]
    fn split_section_handles_multibyte_text() {
        // Byte-vs-char truncation has broken this repo twice. A body of
        // pure multibyte characters is the case that catches it.
        let body = "café ☕ ".repeat(500);
        let parts = split_section(&body, 300);
        for part in &parts {
            assert!(part.chars().count() <= 300);
        }
        assert!(parts.len() > 1);
    }

    #[test]
    fn set_tag_replaces_rather_than_appends() {
        let mut tags = vec!["plan".into(), "status:draft".into(), "phase:2".into()];
        set_tag(&mut tags, STATUS_TAG, "completed");
        assert_eq!(
            tags.iter().filter(|t| t.starts_with(STATUS_TAG)).count(),
            1,
            "two contradictory status tags must never coexist"
        );
        assert!(tags.contains(&"status:completed".to_string()));
        assert!(tags.contains(&"phase:2".to_string()), "other tags survive");
    }

    #[test]
    fn closed_statuses_are_the_ones_that_block_re_execution() {
        assert!(PlanStatus::Completed.is_closed());
        assert!(PlanStatus::Abandoned.is_closed());
        assert!(!PlanStatus::InProgress.is_closed());
        assert!(!PlanStatus::Draft.is_closed());
    }

    #[test]
    fn status_parsing_round_trips() {
        for s in PlanStatus::all() {
            assert_eq!(PlanStatus::parse(s).unwrap().as_str(), *s);
        }
        for s in PhaseStatus::all() {
            assert_eq!(PhaseStatus::parse(s).unwrap().as_str(), *s);
        }
        for s in IceboxStatus::all() {
            assert_eq!(IceboxStatus::parse(s).unwrap().as_str(), *s);
        }
        assert!(PlanStatus::parse("nonsense").is_none());
        assert_eq!(
            PlanStatus::parse("  IN-PROGRESS "),
            Some(PlanStatus::InProgress)
        );
    }

    fn doc_with_phases(phases: &[(usize, PhaseStatus)]) -> PlanDocument {
        PlanDocument {
            slug: "p".into(),
            title: "P".into(),
            overview: String::new(),
            status: PlanStatus::InProgress,
            sections: phases
                .iter()
                .enumerate()
                .map(|(i, (phase, status))| PlanSection {
                    topic_key: plan_section_key("p", i, "s"),
                    index: i,
                    title: "s".into(),
                    content: String::new(),
                    phase: Some(*phase),
                    phase_status: *status,
                })
                .collect(),
        }
    }

    #[test]
    fn phase_progress_counts_phases_not_entries() {
        // Two entries for phase 1 (a split section) is still one phase.
        let doc = doc_with_phases(&[
            (1, PhaseStatus::Done),
            (1, PhaseStatus::Done),
            (2, PhaseStatus::Pending),
        ]);
        assert_eq!(doc.phase_progress(), (1, 2));
    }

    #[test]
    fn a_phase_is_done_only_when_all_its_parts_are() {
        let doc = doc_with_phases(&[(1, PhaseStatus::Done), (1, PhaseStatus::InProgress)]);
        assert_eq!(doc.phase_progress(), (0, 1));
    }

    #[test]
    fn prose_only_plans_report_no_phases_rather_than_zero_of_n() {
        let doc = PlanDocument {
            slug: "p".into(),
            title: "P".into(),
            overview: String::new(),
            status: PlanStatus::Draft,
            sections: vec![PlanSection {
                topic_key: plan_section_key("p", 0, "intro"),
                index: 0,
                title: "Intro".into(),
                content: "prose".into(),
                phase: None,
                phase_status: PhaseStatus::Pending,
            }],
        };
        assert_eq!(doc.phase_progress(), (0, 0));
    }

    // ── Round trips through a real librarian and store ──────────

    #[tokio::test]
    async fn plan_round_trips_through_storage_in_section_order() {
        let lib = test_lib().await;
        let store = PlanStore::new(&lib, None);

        let created = store
            .create_plan(
                "Auth rewrite",
                None,
                "Replace the hand-rolled session check.",
                &[
                    SectionInput::prose("Problem", "sessions are checked in four places"),
                    SectionInput::phase(1, "Extract middleware", "move the check into one layer"),
                    SectionInput::phase(2, "Delete the copies", "remove the other three"),
                ],
                Some(PlanStatus::Draft),
            )
            .await
            .unwrap();
        let slug = created.slug.clone();
        let written = created.entries;

        assert_eq!(slug, "auth-rewrite");
        assert_eq!(written, 4, "one meta entry plus three sections");

        let doc = store
            .get_plan(&slug)
            .await
            .unwrap()
            .expect("plan must load");
        assert_eq!(doc.title, "Auth rewrite");
        assert_eq!(doc.status, PlanStatus::Draft);
        assert_eq!(
            doc.sections
                .iter()
                .map(|s| s.title.as_str())
                .collect::<Vec<_>>(),
            vec!["Problem", "Extract middleware", "Delete the copies"],
            "sections must come back in writing order, not storage order"
        );
        assert_eq!(doc.phase_progress(), (0, 2));
        assert!(doc.render().contains("## Extract middleware"));
    }

    #[tokio::test]
    async fn phase_status_transition_supersedes_in_place() {
        let lib = test_lib().await;
        let store = PlanStore::new(&lib, None);
        let created = store
            .create_plan(
                "Ship sync",
                None,
                "overview",
                &[
                    SectionInput::phase(1, "First", "do the first thing"),
                    SectionInput::phase(2, "Second", "do the second thing"),
                ],
                Some(PlanStatus::Approved),
            )
            .await
            .unwrap();
        let slug = created.slug.clone();

        let doc = store
            .set_plan_status(&slug, None, Some(1), Some(PhaseStatus::Done))
            .await
            .unwrap();

        assert_eq!(doc.phase_progress(), (1, 2));
        assert_eq!(
            doc.sections.len(),
            2,
            "a status change must supersede the section, not add a second one"
        );
        assert_eq!(doc.status, PlanStatus::Approved, "plan status is untouched");
    }

    #[tokio::test]
    async fn completing_a_plan_marks_it_closed_so_it_is_not_re_executed() {
        let lib = test_lib().await;
        let store = PlanStore::new(&lib, None);
        let created = store
            .create_plan(
                "Done work",
                None,
                "overview",
                &[],
                Some(PlanStatus::InProgress),
            )
            .await
            .unwrap();
        let slug = created.slug.clone();

        let doc = store
            .set_plan_status(&slug, Some(PlanStatus::Completed), None, None)
            .await
            .unwrap();

        assert!(doc.status.is_closed());
        let listed = store.list_plans(Some(PlanStatus::Completed)).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert!(store
            .list_plans(Some(PlanStatus::InProgress))
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn list_plans_reports_meta_entries_only_with_progress() {
        let lib = test_lib().await;
        let store = PlanStore::new(&lib, None);
        store
            .create_plan(
                "Plan one",
                None,
                "o",
                &[
                    SectionInput::phase(1, "a", "x"),
                    SectionInput::phase(2, "b", "y"),
                ],
                Some(PlanStatus::InProgress),
            )
            .await
            .unwrap();
        store
            .create_plan("Plan two", None, "o", &[], Some(PlanStatus::Draft))
            .await
            .unwrap();
        store
            .set_plan_status("plan-one", None, Some(1), Some(PhaseStatus::Done))
            .await
            .unwrap();

        let plans = store.list_plans(None).await.unwrap();
        assert_eq!(
            plans.len(),
            2,
            "sections must not be listed as if they were plans"
        );
        let one = plans.iter().find(|p| p.slug == "plan-one").unwrap();
        assert_eq!((one.phases_done, one.phases_total), (1, 2));
    }

    #[tokio::test]
    async fn one_plans_phases_never_leak_into_another() {
        // Found by dogfooding: with two plans stored, `plan list` reported
        // the same merged progress for both — a completed 8/8 plan read as
        // 2/8 once a second, unstarted plan existed. `list_plans` hands
        // every `plan:` entry to `plan_from_entries`, which collected
        // sections without checking whose they were.
        let lib = test_lib().await;
        let store = PlanStore::new(&lib, None);

        store
            .create_plan(
                "First",
                None,
                "o",
                &[
                    SectionInput::phase(1, "a", "x"),
                    SectionInput::phase(2, "b", "y"),
                ],
                Some(PlanStatus::InProgress),
            )
            .await
            .unwrap();
        store
            .create_plan(
                "Second",
                None,
                "o",
                &[
                    SectionInput::phase(1, "c", "x"),
                    SectionInput::phase(2, "d", "y"),
                    SectionInput::phase(3, "e", "z"),
                ],
                Some(PlanStatus::Draft),
            )
            .await
            .unwrap();
        for phase in [1, 2] {
            store
                .set_plan_status("first", None, Some(phase), Some(PhaseStatus::Done))
                .await
                .unwrap();
        }

        let plans = store.list_plans(None).await.unwrap();
        let first = plans.iter().find(|p| p.slug == "first").unwrap();
        let second = plans.iter().find(|p| p.slug == "second").unwrap();
        assert_eq!(
            (first.phases_done, first.phases_total),
            (2, 2),
            "a finished plan must stay finished when another plan exists"
        );
        assert_eq!(
            (second.phases_done, second.phases_total),
            (0, 3),
            "an untouched plan must not inherit the other's completed phases"
        );

        // And the sections themselves must not be merged.
        let doc = store.get_plan("first").await.unwrap().unwrap();
        assert_eq!(doc.sections.len(), 2);
    }

    #[tokio::test]
    async fn a_slug_that_prefixes_another_does_not_claim_its_sections() {
        // `plan:auth` is a prefix of `plan:auth-flow`, so a prefix query
        // alone pulls the longer plan's sections into the shorter one.
        let lib = test_lib().await;
        let store = PlanStore::new(&lib, None);

        store
            .create_plan(
                "auth",
                Some("auth"),
                "o",
                &[SectionInput::prose("Only mine", "x")],
                Some(PlanStatus::Draft),
            )
            .await
            .unwrap();
        store
            .create_plan(
                "auth flow",
                Some("auth-flow"),
                "o",
                &[
                    SectionInput::prose("Theirs one", "y"),
                    SectionInput::prose("Theirs two", "z"),
                ],
                Some(PlanStatus::Draft),
            )
            .await
            .unwrap();

        let doc = store.get_plan("auth").await.unwrap().unwrap();
        assert_eq!(
            doc.sections
                .iter()
                .map(|s| s.title.as_str())
                .collect::<Vec<_>>(),
            vec!["Only mine"],
            "`plan:auth` must not absorb `plan:auth-flow`'s sections"
        );
        assert_eq!(
            store
                .get_plan("auth-flow")
                .await
                .unwrap()
                .unwrap()
                .sections
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn rewriting_a_plan_retires_sections_it_no_longer_has() {
        let lib = test_lib().await;
        let store = PlanStore::new(&lib, None);
        store
            .create_plan(
                "Shrinking",
                None,
                "o",
                &[
                    SectionInput::prose("Keep", "kept"),
                    SectionInput::prose("Drop", "dropped"),
                ],
                Some(PlanStatus::Draft),
            )
            .await
            .unwrap();
        store
            .create_plan(
                "Shrinking",
                None,
                "o",
                &[SectionInput::prose("Keep", "kept")],
                Some(PlanStatus::Draft),
            )
            .await
            .unwrap();

        let doc = store.get_plan("shrinking").await.unwrap().unwrap();
        assert_eq!(
            doc.sections
                .iter()
                .map(|s| s.title.as_str())
                .collect::<Vec<_>>(),
            vec!["Keep"],
            "a removed section must not linger under the plan's prefix"
        );
    }

    #[tokio::test]
    async fn a_section_that_shrinks_drops_its_trailing_parts() {
        let lib = test_lib().await;
        let store = PlanStore::new(&lib, None);
        store
            .create_plan("Parts", None, "o", &[], Some(PlanStatus::Draft))
            .await
            .unwrap();

        // Long enough to split under the default cap? No — force it by
        // asserting on the section count after a long-then-short rewrite,
        // using a small cap via the same splitter the store uses.
        let long = "paragraph body here\n\n".repeat(2000);
        store
            .save_section("parts", 0, "Body", &long, None)
            .await
            .unwrap();
        let after_long = store.get_plan("parts").await.unwrap().unwrap();
        assert!(
            after_long.sections.len() > 1,
            "a {}-char section must have been split",
            long.chars().count()
        );

        store
            .save_section("parts", 0, "Body", "now short", None)
            .await
            .unwrap();
        let after_short = store.get_plan("parts").await.unwrap().unwrap();
        assert_eq!(
            after_short.sections.len(),
            1,
            "trailing parts of the longer version must be retired"
        );
        assert_eq!(after_short.sections[0].content, "now short");
    }

    #[tokio::test]
    async fn icebox_items_round_trip_and_filter_by_status() {
        let lib = test_lib().await;
        let store = PlanStore::new(&lib, None);

        let slug = store
            .add_icebox(
                "Store the icebox in runar",
                "the backlog is invisible to recall",
                None,
                &[],
            )
            .await
            .unwrap();
        store
            .add_icebox(
                "Explore sqlite-vec",
                "evaluate as vector backend",
                None,
                &[],
            )
            .await
            .unwrap();

        assert_eq!(slug, "store-the-icebox-in-runar");
        let item = store.get_icebox(&slug).await.unwrap().unwrap();
        assert_eq!(item.status, IceboxStatus::Open);

        store
            .set_icebox_status(&slug, IceboxStatus::Promoted, Some("icebox-in-runar"))
            .await
            .unwrap();

        let open = store.list_icebox(Some(IceboxStatus::Open)).await.unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].slug, "explore-sqlite-vec");

        let promoted = store
            .list_icebox(Some(IceboxStatus::Promoted))
            .await
            .unwrap();
        assert_eq!(promoted.len(), 1);
        assert_eq!(promoted[0].promoted_to.as_deref(), Some("icebox-in-runar"));
        assert_eq!(
            promoted[0].title, "Store the icebox in runar",
            "a status change must not disturb the item's own content"
        );
    }

    #[tokio::test]
    async fn plans_and_icebox_items_do_not_see_each_other() {
        let lib = test_lib().await;
        let store = PlanStore::new(&lib, None);
        store
            .create_plan("Shared name", None, "o", &[], Some(PlanStatus::Draft))
            .await
            .unwrap();
        store
            .add_icebox("Shared name", "different thing entirely", None, &[])
            .await
            .unwrap();

        assert_eq!(store.list_plans(None).await.unwrap().len(), 1);
        assert_eq!(store.list_icebox(None).await.unwrap().len(), 1);
        assert_eq!(
            store
                .get_icebox("shared-name")
                .await
                .unwrap()
                .unwrap()
                .content,
            "different thing entirely"
        );
    }

    #[tokio::test]
    async fn plan_writes_enqueue_to_the_sync_outbox() {
        // Team sharing is the requirement that put plans in memory_entries
        // rather than a dedicated table. Assert it at the outbox, which is
        // what actually carries them to the remote.
        let _guard = crate::test_support::with_env("RUNAR_STORAGE_LOCAL", "1");
        let storage = Arc::new(SqliteAdapter::in_memory("test").unwrap());
        storage.initialize().await.unwrap();
        let lib = MemoryLibrarian::new(
            storage.clone(),
            Arc::new(DisabledEmbeddingProvider),
            "test",
            None,
        );
        let store = PlanStore::new(&lib, None);

        store
            .create_plan(
                "Syncable",
                None,
                "overview",
                &[SectionInput::prose("One", "body")],
                Some(PlanStatus::Draft),
            )
            .await
            .unwrap();
        store
            .add_icebox("An idea", "worth doing later", None, &[])
            .await
            .unwrap();

        let health = storage.outbox_health(10).await.unwrap();
        assert!(
            health.total() >= 3,
            "meta + section + icebox item must all be queued for the remote, got {}",
            health.total()
        );
    }

    /// Every place `sentinel` survives in the store: keys, titles, contents
    /// and tags of every keyed entry.
    async fn leaks(lib: &MemoryLibrarian, sentinel: &str) -> Vec<String> {
        lib.list_by_topic_prefix(None, "")
            .await
            .unwrap()
            .into_iter()
            .filter(|e| {
                e.topic_key.as_deref().unwrap_or("").contains(sentinel)
                    || e.title.contains(sentinel)
                    || e.content.contains(sentinel)
                    || e.tags.iter().any(|t| t.contains(sentinel))
            })
            .map(|e| format!("{:?}: {}", e.topic_key, e.title))
            .collect()
    }

    #[tokio::test]
    async fn a_private_block_spanning_sections_stores_none_of_its_words() {
        let lib = test_lib().await;
        let parsed = parse_markdown(
            "# P\n\n## A\n\nopen <private>zebra one\n\n## Zebra heading\n\nzebra two</private> after\n\n## B\n\nb\n",
        );
        PlanStore::new(&lib, None)
            .create_plan(
                "P",
                Some("p"),
                &parsed.overview,
                &parsed.sections,
                Some(PlanStatus::Draft),
            )
            .await
            .unwrap();
        assert_eq!(leaks(&lib, "ebra").await, Vec::<String>::new());
    }

    #[tokio::test]
    async fn a_section_title_never_puts_private_words_in_a_key() {
        let lib = test_lib().await;
        let store = PlanStore::new(&lib, None);
        store
            .create_plan(
                "P",
                Some("p"),
                "o",
                &[SectionInput::phase(
                    1,
                    "Phase 1 — <private>zebra</private> rollout",
                    "x",
                )],
                Some(PlanStatus::Draft),
            )
            .await
            .unwrap();
        store
            .save_section("p", 1, "<private>zebra</private> notes", "y", None)
            .await
            .unwrap();
        assert_eq!(leaks(&lib, "zebra").await, Vec::<String>::new());
    }

    #[tokio::test]
    async fn a_private_block_across_a_part_boundary_stays_private() {
        // A block with paragraph breaks inside, opened just before the
        // split point: the first part holds the opening tag, the second the
        // closing one, and redaction ran per part, after the split.
        let lib = test_lib().await;
        let paragraph = "public paragraph text.\n\n";
        let before = paragraph.repeat((content_limit() - 300) / paragraph.len());
        let block = format!(
            "<private>{}zebra closes the block</private>",
            "hidden line.\n\n".repeat(200)
        );
        // Long enough that the section still splits once the block is gone.
        let after = paragraph.repeat(content_limit() / paragraph.len());
        let body = format!("{before}{block}\n\n{after}");
        PlanStore::new(&lib, None)
            .create_plan(
                "P",
                Some("p"),
                "o",
                &[SectionInput::prose("Long", body)],
                Some(PlanStatus::Draft),
            )
            .await
            .unwrap();
        let doc = PlanStore::new(&lib, None)
            .get_plan("p")
            .await
            .unwrap()
            .unwrap();
        assert!(doc.sections.len() > 1, "the section must have been split");
        assert_eq!(leaks(&lib, "zebra").await, Vec::<String>::new());
        assert_eq!(leaks(&lib, "hidden line").await, Vec::<String>::new());
    }

    #[tokio::test]
    async fn a_title_with_a_private_block_needs_an_explicit_slug() {
        let lib = test_lib().await;
        let store = PlanStore::new(&lib, None);
        let title = "<private>zebra</private> merger";
        assert!(store
            .create_plan(title, None, "o", &[], Some(PlanStatus::Draft))
            .await
            .is_err());
        assert!(store.add_icebox(title, "later", None, &[]).await.is_err());
        assert!(store
            .create_plan(
                "Merger",
                Some("<private>zebra</private>"),
                "o",
                &[],
                Some(PlanStatus::Draft)
            )
            .await
            .is_err());
        assert_eq!(leaks(&lib, "zebra").await, Vec::<String>::new());

        store
            .create_plan(title, Some("merger"), "o", &[], Some(PlanStatus::Draft))
            .await
            .unwrap();
        store
            .add_icebox(title, "later", Some("merger-later"), &[])
            .await
            .unwrap();
        assert_eq!(leaks(&lib, "zebra").await, Vec::<String>::new());
        let doc = store.get_plan("merger").await.unwrap().unwrap();
        assert!(doc.title.contains("[redacted]"), "{}", doc.title);
    }

    #[test]
    fn generic_saves_cannot_use_the_plan_stores_scopes() {
        for (t, key) in [
            (EntryType::Plan, None),
            (EntryType::Icebox, None),
            (EntryType::Decision, Some("plan:x")),
            (EntryType::Decision, Some(" Icebox:x")),
            (EntryType::Note, Some("auth:<private>x</private>")),
        ] {
            assert!(check_generic_save(t, key).is_err(), "{t:?} {key:?}");
        }
        for (t, key) in [
            (EntryType::Decision, Some("planning:x")),
            (EntryType::Decision, Some("auth:approach")),
            (EntryType::Note, None),
        ] {
            assert!(check_generic_save(t, key).is_ok(), "{t:?} {key:?}");
        }
    }

    #[tokio::test]
    async fn two_icebox_items_may_share_a_title_and_description() {
        let lib = test_lib().await;
        let store = PlanStore::new(&lib, None);
        for slug in ["one", "two"] {
            store
                .add_icebox("Same idea", "same reason", Some(slug), &[])
                .await
                .unwrap();
        }
        assert_eq!(store.list_icebox(None).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn icebox_slugs_resolve_like_plan_slugs() {
        let lib = test_lib().await;
        let store = PlanStore::new(&lib, None);
        let slug = store
            .add_icebox("Later idea", "worth a look", Some("Later-Idea"), &[])
            .await
            .unwrap();
        assert_eq!(slug, "later-idea");

        assert!(store.get_icebox("Later-Idea").await.unwrap().is_some());
        let item = store
            .set_icebox_status("LATER IDEA", IceboxStatus::Dropped, None)
            .await
            .unwrap();
        assert_eq!(item.slug, "later-idea");
        assert_eq!(item.status, IceboxStatus::Dropped);
    }

    #[tokio::test]
    async fn a_multi_part_phase_keeps_its_status_when_its_part_count_changes() {
        let lib = test_lib().await;
        let store = PlanStore::new(&lib, None);
        let long = "paragraph body here\n\n".repeat(content_limit() / 10);
        store
            .create_plan(
                "P",
                Some("p"),
                "o",
                &[SectionInput::phase(1, "Phase 1 — big", long.clone())],
                None,
            )
            .await
            .unwrap();
        let doc = store
            .set_plan_status("p", None, Some(1), Some(PhaseStatus::Done))
            .await
            .unwrap();
        assert!(doc.sections.len() > 1, "the phase must have been split");

        let short = "now it fits in one part";
        let created = store
            .create_plan(
                "P",
                Some("p"),
                "o",
                &[SectionInput::phase(1, "Phase 1 — big", short)],
                None,
            )
            .await
            .unwrap();
        let doc = store.get_plan("p").await.unwrap().unwrap();
        assert_eq!(doc.sections.len(), 1);
        assert_eq!(doc.phase_progress(), (1, 1));
        assert_eq!(created.done_phases_changed, ["Phase 1 — big"]);
    }

    #[tokio::test]
    async fn a_retag_keeps_the_tags_the_store_does_not_own() {
        let lib = test_lib().await;
        let store = PlanStore::new(&lib, None);
        store
            .create_plan(
                "Tagged",
                Some("tagged"),
                "o",
                &[],
                Some(PlanStatus::Approved),
            )
            .await
            .unwrap();
        let meta = lib
            .get_by_topic_key(None, "plan:tagged")
            .await
            .unwrap()
            .unwrap();
        let mut tags = meta.tags.clone();
        tags.extend(["truncated".to_string(), "keep-me".to_string()]);
        lib.retag(meta.id, tags).await.unwrap();

        // Unchanged text, new status: a duplicate whose tags are rewritten.
        store
            .create_plan(
                "Tagged",
                Some("tagged"),
                "o",
                &[],
                Some(PlanStatus::Completed),
            )
            .await
            .unwrap();
        let after = lib.get(meta.id).await.unwrap();
        assert!(
            after.tags.contains(&"status:completed".to_string()),
            "{:?}",
            after.tags
        );
        assert!(
            !after.tags.contains(&"status:approved".to_string()),
            "{:?}",
            after.tags
        );
        for kept in ["truncated", "keep-me", "plan"] {
            assert!(
                after.tags.contains(&kept.to_string()),
                "{kept}: {:?}",
                after.tags
            );
        }
    }

    /// A topic-key supersede soft-deleted the old meta locally and queued
    /// an insert for the new one only, so the remote kept both live.
    #[tokio::test]
    async fn a_superseded_meta_queues_its_tombstone() {
        let _guard = crate::test_support::with_env("RUNAR_STORAGE_LOCAL", "1");
        let storage = Arc::new(SqliteAdapter::in_memory("test").unwrap());
        storage.initialize().await.unwrap();
        let lib = MemoryLibrarian::new(
            storage.clone(),
            Arc::new(DisabledEmbeddingProvider),
            "test",
            None,
        );
        let store = PlanStore::new(&lib, None);
        store
            .create_plan("Synced", Some("synced"), "v1", &[], None)
            .await
            .unwrap();
        let old = lib
            .get_by_topic_key(None, "plan:synced")
            .await
            .unwrap()
            .unwrap();
        crate::test_support::drain_outbox(&*storage).await;

        store
            .create_plan("Synced", Some("synced"), "v2", &[], None)
            .await
            .unwrap();
        let queued = crate::test_support::drain_outbox(&*storage).await;
        assert!(
            queued
                .iter()
                .any(|e| e.id == old.id && e.deleted_at.is_some()),
            "the old meta's tombstone must reach the outbox: {:?}",
            queued
                .iter()
                .map(|e| (e.id, e.deleted_at))
                .collect::<Vec<_>>()
        );
        assert!(queued
            .iter()
            .any(|e| e.id != old.id && e.deleted_at.is_none() && e.content == "v2"));
    }

    #[tokio::test]
    async fn plan_store_keeps_every_plans_data_on_sqlite() {
        let lib = test_lib().await;
        parity::keeps_every_plans_data(&lib, "parity").await;
    }

    #[tokio::test]
    async fn a_decision_under_an_icebox_key_is_not_an_icebox_item() {
        let lib = test_lib().await;
        let decision = lib
            .propose(MemoryEntryInput {
                title: "Later".into(),
                content: "revisit the queue design".into(),
                entry_type: EntryType::Decision,
                topic_key: Some("icebox:ghost".into()),
                ..Default::default()
            })
            .await
            .unwrap()
            .id;
        let store = PlanStore::new(&lib, None);

        assert!(store.list_icebox(None).await.unwrap().is_empty());
        assert!(store.get_icebox("ghost").await.unwrap().is_none());
        assert!(store
            .set_icebox_status("ghost", IceboxStatus::Dropped, None)
            .await
            .is_err());
        assert!(
            !lib.get(decision)
                .await
                .unwrap()
                .tags
                .iter()
                .any(|t| t.starts_with("status:")),
            "the decision must not be retagged as an icebox item"
        );
    }

    #[tokio::test]
    async fn unknown_slugs_are_errors_rather_than_silent_no_ops() {
        let lib = test_lib().await;
        let store = PlanStore::new(&lib, None);

        assert!(store.get_plan("never-written").await.unwrap().is_none());
        assert!(store.get_icebox("never-written").await.unwrap().is_none());
        assert!(store
            .set_plan_status("never-written", Some(PlanStatus::Completed), None, None)
            .await
            .is_err());
        assert!(store
            .set_icebox_status("never-written", IceboxStatus::Dropped, None)
            .await
            .is_err());

        store
            .create_plan("Real", None, "o", &[], Some(PlanStatus::Draft))
            .await
            .unwrap();
        assert!(
            store
                .set_plan_status("real", None, Some(9), Some(PhaseStatus::Done))
                .await
                .is_err(),
            "advancing a phase that does not exist must fail loudly"
        );
    }
}
