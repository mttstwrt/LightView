//! Arranging the Custom order: placing a file, locking a set into a block,
//! dissolving a block, and returning files to their date positions.
//!
//! The only module that knows both keys and sidecars. Each operation reads the
//! whole order once from the index, plans its writes with a pure function over
//! that snapshot — which is what the tests exercise — and writes sidecars
//! through [`tags::edit_companion`], which re-indexes each one and reports
//! whether the order moved.
//!
//! **Placement hugs a neighbour.** A file dropped behind P gets a key that
//! extends P's own ([`order_key::after`]); one dropped with nothing visible
//! above it hugs the file below ([`order_key::before`]). And the neighbour's
//! key is frozen into its sidecar if it was not stored already: unarranged
//! keys come from dates, and a date can be re-read later, or read differently
//! on another host — a video's container time is read in the host's zone, and
//! a host without `ffprobe` falls back to the file time. A frozen anchor cannot
//! move out from under what was placed against it.
//!
//! **Nothing here runs before the index can place a file** — see
//! [`Gallery::arrangeable`].

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use crate::cache::db::CacheError;
use crate::companion::schema::{CompanionFile, Order};
use crate::path::{PathError, RelPath};
use crate::server::events::Event;
use crate::services::tags::{self, TagError};
use crate::sort::order_key::{self, NoRoom};
use crate::sort::sorter;
use crate::state::Gallery;

#[derive(Debug, thiserror::Error)]
pub enum OrderError {
    #[error("arranging is available once the gallery finishes indexing")]
    StillIndexing,
    #[error("{0} is not in the gallery")]
    NotFound(RelPath),
    #[error("{path} is already in the ordered set \"{block}\"; unlock that set or take it out first")]
    InAnotherBlock { path: RelPath, block: String },
    #[error("a set needs a name")]
    NoName,
    #[error("there is no room to place a file here")]
    NoRoom,
    #[error(transparent)]
    Tag(#[from] TagError),
    #[error(transparent)]
    Cache(#[from] CacheError),
    #[error(transparent)]
    Path(#[from] PathError),
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Join(#[from] tokio::task::JoinError),
}

impl From<NoRoom> for OrderError {
    fn from(_: NoRoom) -> Self {
        OrderError::NoRoom
    }
}

/// One file in the Custom order, as the index holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Row {
    pub path: RelPath,
    /// What the file sorts at: its block's key when it is in one, else its own.
    pub sort_key: String,
    /// Its own key: the stored one, or its default.
    pub own_key: String,
    /// Whether `own_key` is stored in its sidecar rather than derived.
    pub stored: bool,
    pub block: Option<String>,
    pub pos: Option<String>,
}

/// One sidecar change a plan asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Edit {
    /// A place of its own, outside any block.
    Loose(String),
    /// A new key for a block member, which keeps its block and position.
    Key(String),
    /// A new position inside the block, under the same key.
    Pos(String),
    /// Into a block: the set tag, the block's key, a position.
    Lock { set: String, key: String, pos: String },
    /// No place at all: back to where the date order puts it.
    Clear,
}

impl Edit {
    /// Apply to a sidecar; reports whether anything changed.
    fn apply(&self, companion: &mut CompanionFile) -> bool {
        let before = (companion.meta.order.clone(), companion.tags.set.len());
        match self {
            // Replaces the whole order, not just the key: a stale block name
            // left by an older build would otherwise keep the new place from
            // being honoured at all.
            Edit::Loose(key) => {
                companion.meta.order = Some(Order { key: Some(key.clone()), ..Default::default() });
            }
            Edit::Key(key) => companion.meta.order.get_or_insert_default().key = Some(key.clone()),
            Edit::Pos(pos) => companion.meta.order.get_or_insert_default().pos = Some(pos.clone()),
            Edit::Lock { set, key, pos } => {
                if !companion.tags.set.contains(set) {
                    companion.tags.set.push(set.clone());
                    companion.tags.set.sort();
                }
                companion.meta.order = Some(Order {
                    key: Some(key.clone()),
                    set: Some(set.clone()),
                    pos: Some(pos.clone()),
                });
            }
            Edit::Clear => companion.meta.order = None,
        }
        (companion.meta.order.clone(), companion.tags.set.len()) != before
    }
}

type Plan = Vec<(RelPath, Edit)>;

// ---------------------------------------------------------------------------
// The four operations
// ---------------------------------------------------------------------------

/// Move `path` — or its whole block — into the gap between `after` and
/// `before`, the two neighbours the person saw. Either may be absent.
pub async fn place(
    gallery: &Gallery,
    path: &RelPath,
    after: Option<&RelPath>,
    before: Option<&RelPath>,
) -> Result<(), OrderError> {
    ready(gallery)?;
    let rows = load(gallery).await?;
    let plan = plan_place(&rows, path, after, before)?;
    let edited = write(gallery, plan).await?;
    announce(gallery, edited.order_changed, false).await;
    Ok(())
}

/// Lock `paths`, in the order given, into the ordered set `name`. Appends to
/// the end if `name` is already a block. Returns how many files changed.
pub async fn lock_set(gallery: &Gallery, name: &str, paths: &[RelPath]) -> Result<usize, OrderError> {
    ready(gallery)?;
    let name = name.trim();
    if name.is_empty() {
        return Err(OrderError::NoName);
    }
    // Every sidecar is read before the first is written, so a refusal never
    // leaves half a set locked. The index is not asked: it may be behind a
    // sidecar another machine just wrote.
    let current = read_orders(gallery, paths).await?;
    let rows = load(gallery).await?;
    let plan = plan_lock(&rows, &current, name, paths)?;
    let edited = write(gallery, plan).await?;
    let changed = edited.touched.len();
    announce(gallery, edited.order_changed, changed > 0).await;
    Ok(changed)
}

/// Dissolve the block `name`: its members return to their date positions and
/// stay in the set.
pub async fn unlock_set(gallery: &Gallery, name: &str) -> Result<usize, OrderError> {
    ready(gallery)?;
    let rows = load(gallery).await?;
    let plan = plan_unlock(&rows, name);
    let edited = write(gallery, plan).await?;
    announce(gallery, edited.order_changed, false).await;
    Ok(edited.touched.len())
}

/// Return `paths` to their date positions. A block member leaves its block
/// and stays in its set.
pub async fn reset_order(gallery: &Gallery, paths: &[RelPath]) -> Result<usize, OrderError> {
    ready(gallery)?;
    let plan = paths.iter().map(|p| (p.clone(), Edit::Clear)).collect();
    let edited = write(gallery, plan).await?;
    announce(gallery, edited.order_changed, false).await;
    Ok(edited.touched.len())
}

fn ready(gallery: &Gallery) -> Result<(), OrderError> {
    if gallery.arrangeable.load(Ordering::Acquire) {
        Ok(())
    } else {
        Err(OrderError::StillIndexing)
    }
}

/// The whole Custom order, unfiltered. One query of short strings per action.
pub(crate) async fn load(gallery: &Gallery) -> Result<Vec<Row>, OrderError> {
    let own = format!("COALESCE(o.key, {})", order_key::default_key("m"));
    let cols = format!("m.path, COALESCE(b.key, {own}), {own}, o.key IS NOT NULL, o.block, o.pos");
    let sql = sorter::custom_statement(&cols, None);
    let conn = gallery.db.read().await;
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
            r.get(4)?,
            r.get(5)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (path, sort_key, own_key, stored, block, pos) = row?;
        out.push(Row { path: RelPath::new(&path)?, sort_key, own_key, stored, block, pos });
    }
    Ok(out)
}

/// Each path's honoured block, straight from its sidecar.
async fn read_orders(
    gallery: &Gallery,
    paths: &[RelPath],
) -> Result<HashMap<RelPath, Option<String>>, OrderError> {
    let mut resolved = Vec::new();
    for path in paths {
        resolved.push((path.clone(), gallery.root.resolve(path)?));
    }
    let read = tokio::task::spawn_blocking(move || {
        resolved
            .into_iter()
            .map(|(path, absolute)| {
                let block = crate::companion::reader::read_companion(absolute.as_path())
                    .ok()
                    .flatten()
                    .and_then(|c| c.honoured_order().and_then(|o| o.set.clone()));
                (path, block)
            })
            .collect()
    })
    .await?;
    Ok(read)
}

async fn write(gallery: &Gallery, plan: Plan) -> Result<tags::Edited, OrderError> {
    if plan.is_empty() {
        return Ok(tags::Edited::default());
    }
    let paths: Vec<RelPath> = plan.iter().map(|(p, _)| p.clone()).collect();
    let edits: Arc<HashMap<RelPath, Edit>> = Arc::new(plan.into_iter().collect());
    Ok(tags::edit_companion(gallery, &paths, move |path, companion| {
        edits.get(path).is_some_and(|e| e.apply(companion))
    })
    .await?)
}

async fn announce(gallery: &Gallery, order_changed: bool, tags_changed: bool) {
    if order_changed {
        gallery.events.send(Event::OrderChanged);
    }
    if tags_changed {
        // Locking adds the set tag where it was missing.
        let _ = gallery.refresh_autocomplete().await;
        gallery.events.send(Event::TagsIndexed);
    }
}

// ---------------------------------------------------------------------------
// Planning — pure, over a snapshot of the order
// ---------------------------------------------------------------------------

fn index_of(rows: &[Row], path: &RelPath) -> Result<usize, OrderError> {
    rows.iter().position(|r| &r.path == path).ok_or_else(|| OrderError::NotFound(path.clone()))
}

/// A frozen copy of an unarranged anchor's key, so it cannot move away from
/// what was placed against it.
fn freeze(row: &Row) -> Option<(RelPath, Edit)> {
    (!row.stored && row.block.is_none()).then(|| (row.path.clone(), Edit::Loose(row.own_key.clone())))
}

pub(crate) fn plan_place(
    rows: &[Row],
    path: &RelPath,
    after: Option<&RelPath>,
    before: Option<&RelPath>,
) -> Result<Plan, OrderError> {
    if after == Some(path) || before == Some(path) {
        return Ok(Vec::new());
    }
    let me = index_of(rows, path)?;
    let a = after.map(|p| index_of(rows, p)).transpose()?;
    let b = before.map(|p| index_of(rows, p)).transpose()?;
    let my_block = rows[me].block.as_deref();
    let in_mine = |i: usize| my_block.is_some() && rows[i].block.as_deref() == my_block;

    // Inside its own block: a new position, nothing else moves.
    if a.is_some_and(in_mine) || b.is_some_and(in_mine) {
        let members: Vec<usize> = (0..rows.len()).filter(|&i| in_mine(i) && i != me).collect();
        let positioned = |i: &&usize| rows[**i].pos.is_some();
        let (lo, hi) = match a.filter(|&i| in_mine(i)) {
            Some(a) => (
                rows[a].pos.clone().or_else(|| members.iter().rev().find(positioned).and_then(|&i| rows[i].pos.clone())),
                members.iter().find(|&&i| i > a).and_then(|&i| rows[i].pos.clone()),
            ),
            None => {
                let b = b.expect("a gap touching the block has one side in it");
                (
                    members.iter().rev().find(|&&i| i < b).and_then(|&i| rows[i].pos.clone()),
                    rows[b].pos.clone(),
                )
            }
        };
        let pos = match (&lo, &hi) {
            (Some(lo), hi) => order_key::after(lo, hi.as_deref())?,
            (None, Some(hi)) => order_key::before(hi, None)?,
            (None, None) => order_key::spread(None, 1).remove(0),
        };
        return Ok(vec![(path.clone(), Edit::Pos(pos))]);
    }

    // Everything else moves the file, or the block it is in, as one unit.
    let moving: Vec<usize> = match my_block {
        Some(_) => (0..rows.len()).filter(|&i| in_mine(i)).collect(),
        None => vec![me],
    };
    let rest: Vec<usize> = (0..rows.len()).filter(|i| !moving.contains(i)).collect();
    let at = |i: usize| rest.iter().position(|&r| r == i).expect("a neighbour outside the unit");
    let same_block = |x: usize, y: usize| rows[x].block.is_some() && rows[x].block == rows[y].block;

    // Where the unit goes among the others, as an index into `rest`. A
    // neighbour inside a foreign block stands for the whole block, so a drop
    // never lands inside one.
    let slot = match (a, b) {
        (Some(a), _) => {
            let mut k = at(a) + 1;
            while k < rest.len() && same_block(rest[k], a) {
                k += 1;
            }
            k
        }
        (None, Some(b)) => {
            let mut k = at(b);
            while k > 0 && same_block(rest[k - 1], b) {
                k -= 1;
            }
            k
        }
        (None, None) => 0,
    };

    // Already there: nothing to write.
    let unit_first = moving[0];
    let now_after = unit_first.checked_sub(1);
    if now_after == slot.checked_sub(1).map(|k| rest[k]) {
        return Ok(Vec::new());
    }

    let lower = slot.checked_sub(1).map(|k| rest[k]);
    let upper = rest.get(slot).copied();
    let mut plan: Plan = Vec::new();

    // The key, hugging the neighbour the person named: the file above for a
    // drop behind it, the file below for a drop with nothing above.
    let hug_lower = a.is_some();
    let mut key = match (lower, upper) {
        (Some(l), u) if hug_lower || u.is_none() => {
            order_key::after(&rows[l].sort_key, u.map(|u| rows[u].sort_key.as_str()))
        }
        (l, Some(u)) => order_key::before(&rows[u].sort_key, l.map(|l| rows[l].sort_key.as_str())),
        (None, None) => Ok(order_key::spread(None, 1).remove(0)),
        (Some(_), None) => unreachable!("covered above"),
    };

    // Two neighbours with equal keys leave no room between them — a race
    // between two machines placing at one spot. Move the upper one just past
    // the lower first, then place against that.
    if key == Err(NoRoom)
        && let (Some(l), Some(u)) = (lower, upper)
    {
        let beyond = rest.get(slot + 1).map(|&n| rows[n].sort_key.as_str());
        let bumped = order_key::after(&rows[l].sort_key, beyond)?;
        plan.extend(rekey_unit(rows, u, &bumped));
        key = order_key::after(&rows[l].sort_key, Some(&bumped));
    }
    let key = key?;

    // Freeze the neighbour the key was hugged against.
    let anchor = if hug_lower { lower } else { upper.or(lower) };
    if let Some(anchor) = anchor.and_then(|i| freeze(&rows[i])) {
        plan.push(anchor);
    }
    for &i in &moving {
        let edit = if my_block.is_some() { Edit::Key(key.clone()) } else { Edit::Loose(key.clone()) };
        plan.push((rows[i].path.clone(), edit));
    }
    Ok(plan)
}

/// Give the unit at `i` — a loose file, or every member of its block — `key`.
fn rekey_unit(rows: &[Row], i: usize, key: &str) -> Plan {
    match &rows[i].block {
        Some(block) => rows
            .iter()
            .filter(|r| r.block.as_ref() == Some(block))
            .map(|r| (r.path.clone(), Edit::Key(key.to_string())))
            .collect(),
        None => vec![(rows[i].path.clone(), Edit::Loose(key.to_string()))],
    }
}

/// `current` is each path's honoured block, read from its sidecar.
pub(crate) fn plan_lock(
    rows: &[Row],
    current: &HashMap<RelPath, Option<String>>,
    name: &str,
    paths: &[RelPath],
) -> Result<Plan, OrderError> {
    let mut seen = std::collections::HashSet::new();
    let mut joining = Vec::new();
    for path in paths {
        if !seen.insert(path) {
            continue;
        }
        match current.get(path).cloned().flatten() {
            Some(block) if block == name => {} // already locked in
            Some(block) => return Err(OrderError::InAnotherBlock { path: path.clone(), block }),
            None => joining.push(path),
        }
    }
    if joining.is_empty() {
        return Ok(Vec::new());
    }

    let members: Vec<&Row> = rows.iter().filter(|r| r.block.as_deref() == Some(name)).collect();
    let (key, after_pos) = match members.first() {
        // Appending to a block: its key, after its last position.
        Some(first) => {
            let last_pos = members.iter().rev().find_map(|r| r.pos.clone());
            (first.sort_key.clone(), last_pos)
        }
        // A new block sits where the earliest of its files was.
        None => {
            let mut keys = Vec::new();
            for path in &joining {
                keys.push(rows[index_of(rows, path)?].own_key.clone());
            }
            (keys.into_iter().min().expect("at least one file joins"), None)
        }
    };
    let positions = order_key::spread(after_pos.as_deref(), joining.len());
    Ok(joining
        .into_iter()
        .zip(positions)
        .map(|(path, pos)| {
            let edit = Edit::Lock { set: name.to_string(), key: key.clone(), pos };
            (path.clone(), edit)
        })
        .collect())
}

pub(crate) fn plan_unlock(rows: &[Row], name: &str) -> Plan {
    rows.iter()
        .filter(|r| r.block.as_deref() == Some(name))
        .map(|r| (r.path.clone(), Edit::Clear))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sort::order_key::default_key_of;

    fn p(s: &str) -> RelPath {
        RelPath::new(s).unwrap()
    }

    /// A loose, unarranged file at `date`.
    fn loose(path: &str, date: i64) -> Row {
        let key = default_key_of(date, path);
        Row { path: p(path), sort_key: key.clone(), own_key: key, stored: false, block: None, pos: None }
    }

    fn member(path: &str, block: &str, key: &str, pos: &str) -> Row {
        Row {
            path: p(path),
            sort_key: key.to_string(),
            own_key: key.to_string(),
            stored: true,
            block: Some(block.to_string()),
            pos: Some(pos.to_string()),
        }
    }

    /// Apply a plan to the rows and re-sort them the way the statement does.
    fn after_plan(rows: &[Row], plan: &Plan) -> Vec<String> {
        let mut rows: Vec<Row> = rows.to_vec();
        for (path, edit) in plan {
            let r = rows.iter_mut().find(|r| &r.path == path).unwrap();
            match edit {
                Edit::Loose(k) => {
                    r.own_key = k.clone();
                    r.stored = true;
                    r.block = None;
                    r.pos = None;
                }
                Edit::Key(k) => r.own_key = k.clone(),
                Edit::Pos(pos) => r.pos = Some(pos.clone()),
                Edit::Lock { set, key, pos } => {
                    r.own_key = key.clone();
                    r.block = Some(set.clone());
                    r.pos = Some(pos.clone());
                }
                Edit::Clear => panic!("not used here"),
            }
        }
        let mut block_key: HashMap<String, String> = HashMap::new();
        for r in &rows {
            if let Some(b) = &r.block {
                let e = block_key.entry(b.clone()).or_insert_with(|| r.own_key.clone());
                if r.own_key < *e {
                    *e = r.own_key.clone();
                }
            }
        }
        rows.sort_by(|x, y| {
            let kx = x.block.as_ref().map_or(&x.own_key, |b| &block_key[b]);
            let ky = y.block.as_ref().map_or(&y.own_key, |b| &block_key[b]);
            (kx, &x.block, x.pos.is_none(), &x.pos, &x.path).cmp(&(ky, &y.block, y.pos.is_none(), &y.pos, &y.path))
        });
        rows.into_iter().map(|r| r.path.as_str().to_string()).collect()
    }

    fn gallery() -> Vec<Row> {
        // Newest first: a, b, then the block [p1 p2 p3] locked at c's old
        // place, then d, e.
        let block_key = default_key_of(300, "c.jpg");
        vec![
            loose("a.jpg", 500),
            loose("b.jpg", 400),
            member("p1.jpg", "comic", &block_key, "1"),
            member("p2.jpg", "comic", &block_key, "2"),
            member("p3.jpg", "comic", &block_key, "3"),
            loose("d.jpg", 200),
            loose("e.jpg", 100),
        ]
    }

    #[test]
    fn a_file_dropped_behind_another_lands_right_behind_it() {
        let rows = gallery();
        let plan = plan_place(&rows, &p("e.jpg"), Some(&p("a.jpg")), Some(&p("b.jpg"))).unwrap();
        assert_eq!(after_plan(&rows, &plan), ["a.jpg", "e.jpg", "b.jpg", "p1.jpg", "p2.jpg", "p3.jpg", "d.jpg"]);
        // The anchor's key is frozen, and the placed key extends it.
        assert!(plan.contains(&(p("a.jpg"), Edit::Loose(rows[0].own_key.clone()))));
        let Some((_, Edit::Loose(k))) = plan.iter().find(|(path, _)| path == &p("e.jpg")) else { panic!() };
        assert!(k.starts_with(&rows[0].own_key));
    }

    #[test]
    fn a_drop_with_nothing_above_hugs_the_file_below() {
        // Under a filter the top of the view is `d`: only `before` is sent.
        let rows = gallery();
        let plan = plan_place(&rows, &p("a.jpg"), None, Some(&p("d.jpg"))).unwrap();
        assert_eq!(after_plan(&rows, &plan), ["b.jpg", "p1.jpg", "p2.jpg", "p3.jpg", "a.jpg", "d.jpg", "e.jpg"]);
    }

    #[test]
    fn a_drop_at_the_very_top_stays_below_later_arrivals() {
        let rows = gallery();
        let plan = plan_place(&rows, &p("e.jpg"), None, Some(&p("a.jpg"))).unwrap();
        let order = after_plan(&rows, &plan);
        assert_eq!(order[0], "e.jpg");
        let Some((_, Edit::Loose(k))) = plan.iter().find(|(path, _)| path == &p("e.jpg")) else { panic!() };
        // A file that arrives tomorrow sorts above it — the user's decision.
        assert!(default_key_of(600, "z.jpg") < *k);
    }

    #[test]
    fn a_drop_next_to_a_foreign_block_never_lands_inside_it() {
        let rows = gallery();
        // Dropped behind the block's first page: snaps behind the whole block.
        let plan = plan_place(&rows, &p("a.jpg"), Some(&p("p1.jpg")), Some(&p("p2.jpg"))).unwrap();
        assert_eq!(after_plan(&rows, &plan), ["b.jpg", "p1.jpg", "p2.jpg", "p3.jpg", "a.jpg", "d.jpg", "e.jpg"]);
        // Dropped in front of its last page: snaps in front of the whole block.
        let plan = plan_place(&rows, &p("e.jpg"), None, Some(&p("p3.jpg"))).unwrap();
        assert_eq!(after_plan(&rows, &plan), ["a.jpg", "b.jpg", "e.jpg", "p1.jpg", "p2.jpg", "p3.jpg", "d.jpg"]);
    }

    #[test]
    fn a_member_dropped_inside_its_block_reorders_the_set() {
        let rows = gallery();
        let plan = plan_place(&rows, &p("p3.jpg"), Some(&p("p1.jpg")), Some(&p("p2.jpg"))).unwrap();
        assert_eq!(plan.len(), 1, "one write: {plan:?}");
        assert_eq!(after_plan(&rows, &plan), ["a.jpg", "b.jpg", "p1.jpg", "p3.jpg", "p2.jpg", "d.jpg", "e.jpg"]);
        // At the block's start: the gap before its first page touches it.
        let plan = plan_place(&rows, &p("p3.jpg"), Some(&p("b.jpg")), Some(&p("p1.jpg"))).unwrap();
        assert_eq!(after_plan(&rows, &plan), ["a.jpg", "b.jpg", "p3.jpg", "p1.jpg", "p2.jpg", "d.jpg", "e.jpg"]);
    }

    #[test]
    fn a_member_dropped_outside_its_block_moves_the_whole_block() {
        let rows = gallery();
        let plan = plan_place(&rows, &p("p2.jpg"), Some(&p("d.jpg")), Some(&p("e.jpg"))).unwrap();
        assert_eq!(after_plan(&rows, &plan), ["a.jpg", "b.jpg", "d.jpg", "p1.jpg", "p2.jpg", "p3.jpg", "e.jpg"]);
        assert!(plan.iter().filter(|(_, e)| matches!(e, Edit::Key(_))).count() == 3);
    }

    #[test]
    fn a_drop_under_a_filter_lands_right_behind_the_visible_neighbour() {
        // A filter hides `b`: the person sees a, d and drops e between them.
        // It lands right behind `a`, ahead of the hidden `b`.
        let rows = gallery();
        let plan = plan_place(&rows, &p("e.jpg"), Some(&p("a.jpg")), Some(&p("d.jpg"))).unwrap();
        assert_eq!(after_plan(&rows, &plan)[..3], ["a.jpg", "e.jpg", "b.jpg"]);
    }

    #[test]
    fn a_drop_where_the_file_already_is_writes_nothing() {
        let rows = gallery();
        assert!(plan_place(&rows, &p("b.jpg"), Some(&p("a.jpg")), None).unwrap().is_empty());
        assert!(plan_place(&rows, &p("a.jpg"), None, Some(&p("b.jpg"))).unwrap().is_empty());
        assert!(plan_place(&rows, &p("a.jpg"), Some(&p("a.jpg")), None).unwrap().is_empty());
    }

    #[test]
    fn equal_neighbours_are_pried_apart_first() {
        // Two machines placed x and y behind `a` at the same moment and came up
        // with one key. Dropping e between them must still land between.
        let mut rows = gallery();
        let k = format!("{}V", rows[0].own_key);
        for (i, name) in [(1, "x.jpg"), (2, "y.jpg")] {
            rows.insert(i, Row { path: p(name), sort_key: k.clone(), own_key: k.clone(), stored: true, block: None, pos: None });
        }
        let plan = plan_place(&rows, &p("e.jpg"), Some(&p("x.jpg")), Some(&p("y.jpg"))).unwrap();
        let order = after_plan(&rows, &plan);
        let at = |n: &str| order.iter().position(|o| o == n).unwrap();
        assert!(at("x.jpg") < at("e.jpg") && at("e.jpg") < at("y.jpg"), "{order:?}");
    }

    #[test]
    fn locking_refuses_a_file_already_in_another_block_before_writing_anything() {
        let rows = gallery();
        let current: HashMap<RelPath, Option<String>> =
            [(p("a.jpg"), None), (p("p1.jpg"), Some("comic".to_string()))].into_iter().collect();
        let err = plan_lock(&rows, &current, "zine", &[p("a.jpg"), p("p1.jpg")]).unwrap_err();
        assert!(matches!(err, OrderError::InAnotherBlock { ref block, .. } if block == "comic"));
    }

    #[test]
    fn a_new_block_sits_where_its_earliest_file_was() {
        let rows = gallery();
        let current: HashMap<RelPath, Option<String>> =
            [(p("e.jpg"), None), (p("b.jpg"), None)].into_iter().collect();
        let plan = plan_lock(&rows, &current, "zine", &[p("e.jpg"), p("b.jpg")]).unwrap();
        // In the order given, at b's place (the earlier of the two).
        assert_eq!(after_plan(&rows, &plan), ["a.jpg", "e.jpg", "b.jpg", "p1.jpg", "p2.jpg", "p3.jpg", "d.jpg"]);
    }

    #[test]
    fn locking_into_an_existing_block_appends() {
        let rows = gallery();
        let current: HashMap<RelPath, Option<String>> =
            [(p("a.jpg"), None), (p("p1.jpg"), Some("comic".to_string()))].into_iter().collect();
        let plan = plan_lock(&rows, &current, "comic", &[p("a.jpg"), p("p1.jpg")]).unwrap();
        assert_eq!(plan.len(), 1, "p1 is already in");
        assert_eq!(after_plan(&rows, &plan), ["b.jpg", "p1.jpg", "p2.jpg", "p3.jpg", "a.jpg", "d.jpg", "e.jpg"]);
    }

    #[test]
    fn unlocking_clears_every_member() {
        let plan = plan_unlock(&gallery(), "comic");
        assert_eq!(plan.len(), 3);
        assert!(plan.iter().all(|(_, e)| *e == Edit::Clear));
    }

    // -----------------------------------------------------------------------
    // End to end, over real sidecars and the real statement
    // -----------------------------------------------------------------------

    async fn gallery_of(dir: &std::path::Path, files: &[(&str, i64)]) -> Gallery {
        let gallery = crate::services::test_gallery(dir);
        let conn = gallery.db.writer().await;
        for (name, date) in files {
            std::fs::write(dir.join(name), b"x").unwrap();
            conn.execute(
                "INSERT INTO media_meta (path, media_type, file_size, mtime) VALUES (?1, 'image', 1, ?2)",
                rusqlite::params![name, date],
            )
            .unwrap();
        }
        drop(conn);
        gallery
    }

    async fn custom(gallery: &Gallery) -> Vec<String> {
        let request = crate::services::media::ItemsRequest {
            sort: sorter::SortField::Custom,
            ..Default::default()
        };
        crate::services::media::get_items(gallery, &request)
            .await
            .unwrap()
            .items
            .into_iter()
            .map(|i| i.path.as_str().to_string())
            .collect()
    }

    #[tokio::test]
    async fn nothing_is_arranged_until_the_index_can_place_it() {
        let d = tempfile::tempdir().unwrap();
        let gallery = gallery_of(d.path(), &[("a.jpg", 2), ("b.jpg", 1)]).await;
        let err = place(&gallery, &p("b.jpg"), None, Some(&p("a.jpg"))).await.unwrap_err();
        assert!(matches!(err, OrderError::StillIndexing));
    }

    #[tokio::test]
    async fn arranging_survives_in_the_sidecars() {
        let d = tempfile::tempdir().unwrap();
        let gallery = gallery_of(d.path(), &[("a.jpg", 5), ("b.jpg", 4), ("c.jpg", 3), ("d.jpg", 2), ("e.jpg", 1)]).await;
        gallery.arrangeable.store(true, Ordering::Release);
        let mut events = gallery.events.subscribe();
        assert_eq!(custom(&gallery).await, ["a.jpg", "b.jpg", "c.jpg", "d.jpg", "e.jpg"]);

        // Lock e, c into a block — in that order — and drop a behind it.
        lock_set(&gallery, "comic", &[p("e.jpg"), p("c.jpg")]).await.unwrap();
        assert_eq!(custom(&gallery).await, ["a.jpg", "b.jpg", "e.jpg", "c.jpg", "d.jpg"]);
        place(&gallery, &p("a.jpg"), Some(&p("c.jpg")), Some(&p("d.jpg"))).await.unwrap();
        let arranged = custom(&gallery).await;
        assert_eq!(arranged, ["b.jpg", "e.jpg", "c.jpg", "a.jpg", "d.jpg"]);
        assert!(std::iter::from_fn(|| events.try_recv().ok()).any(|e| matches!(e, Event::OrderChanged)));

        // Throw the index away and rebuild it from the sidecars alone.
        {
            let conn = gallery.db.writer().await;
            crate::cache::index::clear(&conn).unwrap();
        }
        let presence = Arc::new(crate::util::presence::Presence::default());
        crate::services::gallery::reindex_companions(&gallery, &presence).await.unwrap();
        assert_eq!(custom(&gallery).await, arranged);

        // And back again.
        unlock_set(&gallery, "comic").await.unwrap();
        reset_order(&gallery, &[p("a.jpg"), p("b.jpg")]).await.unwrap();
        assert_eq!(custom(&gallery).await, ["a.jpg", "b.jpg", "c.jpg", "d.jpg", "e.jpg"]);
    }
}
