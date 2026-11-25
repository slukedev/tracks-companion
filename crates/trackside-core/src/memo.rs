//! Derived results kept between reads, so suggestions are not rebuilt from every listen and
//! song on each request.
//!
//! Every entry is tagged with the database's generation ([`crate::Db::generation`]), which
//! moves after each committed write that changes what suggestions read: any recorded change
//! but play queues and acquisitions, plus writes that call [`crate::WriteTx::touch`] (listens,
//! Last.fm data, artwork looked up for suggestions). An entry from an older generation is never
//! used, so an answer from here is always the answer a fresh computation would give.
//!
//! Some entries depend on the library alone (songs, albums, artists, what the caller may
//! see) and are tagged with [`crate::Db::library_generation`] instead, which moves only with
//! those changes, so a listen does not throw them away.
//!
//! The memo is reachable only inside [`crate::Db::read`] and [`crate::Db::read_blocking`],
//! which read the generation before their snapshot starts. A result can therefore only be
//! stored under a generation older than its data, never newer. Outside a read (in a write
//! transaction, for one) nothing is cached.

use std::any::Any;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use crate::db::DbError;
use crate::ids::UserKey;

/// Entries are weighed (about one unit per song held); past this the least recently used go.
const MAX_WEIGHT: usize = 200_000;
/// [`per_user`] keeps values for at most this many users of each kind, the most recent.
const MAX_USERS: usize = 16;

type Value = Arc<dyn Any + Send + Sync>;

struct Entry {
    generation: u64,
    value: Value,
    weight: usize,
    used: u64,
}

#[derive(Default)]
struct Entries {
    map: HashMap<(&'static str, String), Entry>,
    /// Per user: what [`per_user`] holds, with the marker it was built for and when it was
    /// last used.
    users: HashMap<(&'static str, UserKey), (i64, Value, u64)>,
    weight: usize,
    tick: u64,
}

/// The cache. One per [`crate::Db`].
#[derive(Default)]
pub struct Memo {
    entries: Mutex<Entries>,
}

impl std::fmt::Debug for Memo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Memo").finish_non_exhaustive()
    }
}

impl Memo {
    fn lock(&self) -> std::sync::MutexGuard<'_, Entries> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn get(&self, kind: &'static str, key: &str, generation: u64) -> Option<Value> {
        let mut entries = self.lock();
        entries.tick += 1;
        let tick = entries.tick;
        let entry = entries.map.get_mut(&(kind, key.to_owned()))?;
        if entry.generation != generation {
            return None;
        }
        entry.used = tick;
        Some(Arc::clone(&entry.value))
    }

    fn put(&self, kind: &'static str, key: String, generation: u64, value: Value, weight: usize) {
        let mut entries = self.lock();
        entries.tick += 1;
        let used = entries.tick;
        let weight = weight + 1;
        if let Some(old) = entries.map.insert(
            (kind, key),
            Entry {
                generation,
                value,
                weight,
                used,
            },
        ) {
            entries.weight -= old.weight;
        }
        entries.weight += weight;
        if entries.weight > MAX_WEIGHT {
            // The least recently used go first.
            let mut order: Vec<((&'static str, String), u64)> = entries
                .map
                .iter()
                .map(|(k, e)| (k.clone(), e.used))
                .collect();
            order.sort_by_key(|(_, used)| *used);
            for (key, _) in order {
                if entries.weight <= MAX_WEIGHT / 2 {
                    break;
                }
                if let Some(old) = entries.map.remove(&key) {
                    entries.weight -= old.weight;
                }
            }
        }
    }

    /// Drops everything. For tests and memory pressure.
    pub fn clear(&self) {
        *self.lock() = Entries::default();
    }

    /// How many entries are held.
    pub fn len(&self) -> usize {
        self.lock().map.len()
    }

    /// Whether nothing is held.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The generations a read started at: everything, and the library alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Generations {
    pub all: u64,
    pub library: u64,
}

thread_local! {
    static CURRENT: RefCell<Option<(Arc<Memo>, Generations)>> = const { RefCell::new(None) };
}

/// Runs `f` with `memo` reachable at `generation`, restoring what was there before.
pub(crate) fn scope<T>(memo: &Arc<Memo>, generation: Generations, f: impl FnOnce() -> T) -> T {
    struct Restore(Option<(Arc<Memo>, Generations)>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let previous = self.0.take();
            CURRENT.with(|c| *c.borrow_mut() = previous);
        }
    }
    let previous = CURRENT.with(|c| c.borrow_mut().replace((Arc::clone(memo), generation)));
    let _restore = Restore(previous);
    f()
}

/// What a result for `a` depends on of the caller: who they are and what they may see.
pub(crate) fn access_key(a: &crate::Access) -> String {
    format!(
        "{}:{:?}:{:?}:{}",
        a.user.0, a.role, a.libraries, a.sync_epoch
    )
}

fn current() -> Option<(Arc<Memo>, Generations)> {
    CURRENT.with(|c| c.borrow().clone())
}

/// What was stored under `kind` and `key` at the current library generation. Kinds used
/// here are never used with [`cached`].
pub(crate) fn library_get<T: Send + Sync + 'static>(
    kind: &'static str,
    key: &str,
) -> Option<Arc<T>> {
    let (memo, generation) = current()?;
    memo.get(kind, key, generation.library)?
        .downcast::<T>()
        .ok()
}

/// Stores a value that depends on the library alone, at the current library generation.
pub(crate) fn library_put<T: Send + Sync + 'static>(
    kind: &'static str,
    key: String,
    value: Arc<T>,
    weight: usize,
) {
    if let Some((memo, generation)) = current() {
        memo.put(kind, key, generation.library, value, weight);
    }
}

/// `compute()`, or what it gave before at this generation for the same `kind` and `key`.
/// `weight` is about how many songs the value holds.
pub(crate) fn cached<T>(
    kind: &'static str,
    key: impl FnOnce() -> String,
    weight: impl FnOnce(&T) -> usize,
    compute: impl FnOnce() -> Result<T, DbError>,
) -> Result<T, DbError>
where
    T: Clone + Send + Sync + 'static,
{
    let Some((memo, generation)) = current() else {
        return compute();
    };
    let generation = generation.all;
    let key = key();
    if let Some(hit) = memo
        .get(kind, &key, generation)
        .and_then(|v| v.downcast::<T>().ok())
    {
        return Ok((*hit).clone());
    }
    let value = compute()?;
    let size = weight(&value);
    memo.put(kind, key, generation, Arc::new(value.clone()), size);
    Ok(value)
}

/// One user's value, kept while `marker` (read in the current snapshot) is unchanged,
/// whatever else changes. `update` gets the value built for an older marker, if any, to
/// bring it up to date instead of building it again; it is no longer shared then unless
/// another read still holds it.
pub(crate) fn per_user<T>(
    kind: &'static str,
    user: UserKey,
    marker: i64,
    update: impl FnOnce(Option<(i64, Arc<T>)>) -> Result<T, DbError>,
) -> Result<Arc<T>, DbError>
where
    T: Send + Sync + 'static,
{
    let Some((memo, _)) = current() else {
        return update(None).map(Arc::new);
    };
    let held = {
        let mut entries = memo.lock();
        entries.tick += 1;
        let tick = entries.tick;
        let stored = entries.users.get_mut(&(kind, user)).map(|(m, _, used)| {
            *used = tick;
            *m
        });
        match stored {
            Some(m) if m == marker => entries
                .users
                .get(&(kind, user))
                .and_then(|(_, v, _)| Arc::clone(v).downcast::<T>().ok())
                .map(|v| (m, v)),
            // Taken out, so the update can usually change it in place.
            Some(m) if m < marker => entries
                .users
                .remove(&(kind, user))
                .and_then(|(_, v, _)| v.downcast::<T>().ok())
                .map(|v| (m, v)),
            _ => None,
        }
    };
    if let Some((m, value)) = &held
        && *m == marker
    {
        return Ok(Arc::clone(value));
    }
    let older = held;
    let value = Arc::new(update(older)?);
    let mut entries = memo.lock();
    let newer = entries
        .users
        .get(&(kind, user))
        .is_some_and(|(m, _, _)| *m > marker);
    if !newer {
        entries.tick += 1;
        let (stored, now): (Value, u64) = (value.clone(), entries.tick);
        entries.users.insert((kind, user), (marker, stored, now));
        let mut same: Vec<(u64, UserKey)> = entries
            .users
            .iter()
            .filter(|((k, _), _)| *k == kind)
            .map(|((_, u), (_, _, used))| (*used, *u))
            .collect();
        if same.len() > MAX_USERS {
            same.sort_unstable();
            for (_, u) in &same[..same.len() - MAX_USERS] {
                entries.users.remove(&(kind, *u));
            }
        }
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_kept_per_generation_only() -> Result<(), DbError> {
        let memo = Arc::new(Memo::default());
        let mut computed = 0;
        let get = |generation: u64, computed: &mut i32| {
            let generation = Generations {
                all: generation,
                library: 0,
            };
            scope(&memo, generation, || {
                cached(
                    "t",
                    || String::from("k"),
                    |_: &i32| 1,
                    || {
                        *computed += 1;
                        Ok(*computed)
                    },
                )
            })
        };
        assert_eq!(get(1, &mut computed)?, 1);
        assert_eq!(get(1, &mut computed)?, 1, "kept");
        assert_eq!(get(2, &mut computed)?, 2, "a new generation computes again");
        assert_eq!(
            cached("t", || String::from("k"), |_: &i32| 1, || Ok(9))?,
            9,
            "nothing is cached outside a read"
        );
        Ok(())
    }

    #[test]
    fn the_cache_stays_bounded() -> Result<(), DbError> {
        let memo = Arc::new(Memo::default());
        let generation = Generations { all: 1, library: 1 };
        scope(&memo, generation, || {
            for i in 0..100 {
                cached("t", || i.to_string(), |_: &usize| MAX_WEIGHT / 20, || Ok(i))?;
            }
            Ok::<_, DbError>(())
        })?;
        assert!(memo.lock().weight <= MAX_WEIGHT);
        assert!(memo.len() < 100);
        Ok(())
    }

    #[test]
    fn per_user_values_follow_their_marker_and_stay_bounded() -> Result<(), DbError> {
        let memo = Arc::new(Memo::default());
        let generation = Generations { all: 1, library: 1 };
        scope(&memo, generation, || {
            let first = per_user("t", UserKey(1), 5, |older: Option<(i64, Arc<i64>)>| {
                assert!(older.is_none());
                Ok(5)
            })?;
            assert_eq!(*first, 5);
            let same = per_user("t", UserKey(1), 5, |_: Option<(i64, Arc<i64>)>| Ok(-1))?;
            assert_eq!(*same, 5, "kept while the marker holds");
            let grown = per_user("t", UserKey(1), 7, |older: Option<(i64, Arc<i64>)>| {
                Ok(older.map_or(0, |(m, v)| *v + 7 - m))
            })?;
            assert_eq!(*grown, 7, "brought up to date from the older value");
            for user in 2..40 {
                per_user("t", UserKey(user), 1, |_: Option<(i64, Arc<i64>)>| Ok(1))?;
            }
            Ok::<_, DbError>(())
        })?;
        assert!(memo.lock().users.len() <= MAX_USERS);
        Ok(())
    }
}
