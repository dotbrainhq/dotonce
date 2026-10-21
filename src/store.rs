use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::index::Index;

/// Open on-disk indexes kept at once. Each holds a few file handles and the
/// runner works through users one after another, so older ones are closed.
pub const MAX_OPEN: usize = 64;

const NAME_CHARS: usize = 64;

pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Readable part of the user id plus a hash of the whole id, so ids that read
/// the same after cleaning ("u/1", "u_1") still get their own file.
pub fn file_name(user: &str) -> String {
    let safe: String = user
        .chars()
        .take(NAME_CHARS)
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("{safe}_{:016x}.sqlite", fnv1a64(user.as_bytes()))
}

/// One index per user, so ranking statistics never mix users.
pub struct Store {
    dir: Option<PathBuf>,
    fresh: bool,
    max_open: usize,
    tick: u64,
    open: HashMap<String, (Index, u64)>,
    seen: HashSet<String>,
    wipe_on_open: HashSet<String>,
}

impl Store {
    /// `dir` of ":memory:" gives every user an in-memory index.
    pub fn open(dir: &str, fresh: bool) -> Result<Self> {
        Self::with_max_open(dir, fresh, MAX_OPEN)
    }

    pub fn with_max_open(dir: &str, fresh: bool, max_open: usize) -> Result<Self> {
        let dir = if dir == ":memory:" {
            None
        } else {
            std::fs::create_dir_all(dir).with_context(|| format!("create index dir {dir}"))?;
            Some(PathBuf::from(dir))
        };
        Ok(Self {
            dir,
            fresh,
            max_open: max_open.max(1),
            tick: 0,
            open: HashMap::new(),
            seen: HashSet::new(),
            wipe_on_open: HashSet::new(),
        })
    }

    pub fn index(&mut self, user: &str) -> Result<&mut Index> {
        self.tick += 1;
        let tick = self.tick;
        if !self.open.contains_key(user) {
            if let Some(dir) = &self.dir {
                if self.open.len() >= self.max_open {
                    let oldest = self
                        .open
                        .iter()
                        .min_by_key(|(_, (_, t))| *t)
                        .map(|(u, _)| u.clone());
                    if let Some(u) = oldest {
                        self.open.remove(&u);
                    }
                }
                let path = dir.join(file_name(user));
                let ix = Index::open(&path.to_string_lossy())?;
                let first = self.seen.insert(user.to_string());
                if (first && self.fresh) || self.wipe_on_open.remove(user) {
                    ix.reset()?;
                }
                self.open.insert(user.to_string(), (ix, tick));
            } else {
                self.seen.insert(user.to_string());
                self.open
                    .insert(user.to_string(), (Index::open(":memory:")?, tick));
            }
        }
        let entry = self.open.get_mut(user).expect("opened above");
        entry.1 = tick;
        Ok(&mut entry.0)
    }

    /// Wipes every user opened in this process, including ones closed since.
    pub fn reset(&mut self) -> Result<()> {
        for (ix, _) in self.open.values() {
            ix.reset()?;
        }
        for u in &self.seen {
            if !self.open.contains_key(u) {
                self.wipe_on_open.insert(u.clone());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{SessionDoc, TurnIn};
    use crate::retrieve::{Options, retrieve};

    fn doc(id: &str, user: &str) -> SessionDoc {
        SessionDoc {
            id: id.to_string(),
            user_id: Some(user.to_string()),
            timestamp: Some("2023-01-01T10:00:00+00:00".to_string()),
            turns: vec![TurnIn {
                speaker: "user".to_string(),
                text: "I adopted a dog named Rex".to_string(),
            }],
        }
    }

    fn found(st: &mut Store, user: &str) -> usize {
        retrieve(
            st.index(user).unwrap(),
            "dog",
            user,
            None,
            &Options::default(),
        )
        .unwrap()
        .len()
    }

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dotonce-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn fnv1a64_matches_reference_values() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn file_names_are_clean_and_distinct() {
        let a = file_name("u/1");
        let b = file_name("u_1");
        assert!(a.starts_with("u_1_") && a.ends_with(".sqlite"), "{a}");
        assert_ne!(a, b);
        assert!(file_name("").starts_with('_'));
        assert!(file_name(&"x".repeat(500)).len() < 100);
    }

    #[test]
    fn memory_users_are_separate() {
        let mut st = Store::open(":memory:", false).unwrap();
        st.index("a")
            .unwrap()
            .add_sessions(&[doc("s1", "a")])
            .unwrap();
        assert_eq!(found(&mut st, "a"), 1);
        assert_eq!(found(&mut st, "b"), 0);
        st.reset().unwrap();
        assert_eq!(found(&mut st, "a"), 0);
    }

    #[test]
    fn closed_users_keep_their_data_and_fresh_wipes_once() {
        let dir = temp_dir("evict");
        let d = dir.to_str().unwrap();
        let mut st = Store::with_max_open(d, true, 1).unwrap();
        st.index("a")
            .unwrap()
            .add_sessions(&[doc("s1", "a")])
            .unwrap();
        st.index("b")
            .unwrap()
            .add_sessions(&[doc("s2", "b")])
            .unwrap();
        assert_eq!(found(&mut st, "a"), 1);
        assert_eq!(found(&mut st, "b"), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reset_wipes_closed_users_too() {
        let dir = temp_dir("reset");
        let d = dir.to_str().unwrap();
        let mut st = Store::with_max_open(d, false, 1).unwrap();
        st.index("a")
            .unwrap()
            .add_sessions(&[doc("s1", "a")])
            .unwrap();
        st.index("b")
            .unwrap()
            .add_sessions(&[doc("s2", "b")])
            .unwrap();
        st.reset().unwrap();
        assert_eq!(found(&mut st, "a"), 0);
        st.index("a")
            .unwrap()
            .add_sessions(&[doc("s1", "a")])
            .unwrap();
        assert_eq!(found(&mut st, "b"), 0);
        assert_eq!(found(&mut st, "a"), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
