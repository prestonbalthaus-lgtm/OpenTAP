//! An in-memory [`SinkStore`] (Step 6C): the replication sink's records,
//! kept outside the simulated process so a simulated crash leaves them as
//! a crash leaves the files of `dbladed`'s `--state-dir`. Clones share the
//! records; a save replaces a record whole, as the atomic rename does.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use otap_cp::SinkStore;

/// Records by key, shared by every clone.
#[derive(Debug, Clone, Default)]
pub struct MemStore {
    records: Rc<RefCell<BTreeMap<String, Vec<u8>>>>,
    saves: Rc<RefCell<u64>>,
    fail_saves: Rc<RefCell<bool>>,
}

impl MemStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The record under `key`, if any.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<Vec<u8>> {
        self.records.borrow().get(key).cloned()
    }

    /// Replaces or removes a record behind the sink's back (a lost or a
    /// damaged state directory).
    pub fn put(&self, key: &str, bytes: Option<Vec<u8>>) {
        let mut records = self.records.borrow_mut();
        match bytes {
            Some(b) => {
                records.insert(key.to_string(), b);
            }
            None => {
                records.remove(key);
            }
        }
    }

    /// Forgets every record (a tmpfs state directory after a reboot).
    pub fn clear(&self) {
        self.records.borrow_mut().clear();
    }

    /// Saves made so far.
    #[must_use]
    pub fn saves(&self) -> u64 {
        *self.saves.borrow()
    }

    /// Makes every later save fail (a full or read-only state directory),
    /// or succeed again.
    pub fn set_failing(&self, failing: bool) {
        *self.fail_saves.borrow_mut() = failing;
    }
}

impl SinkStore for MemStore {
    fn load(&mut self, key: &str) -> Result<Option<Vec<u8>>, String> {
        Ok(self.records.borrow().get(key).cloned())
    }

    fn save(&mut self, key: &str, bytes: &[u8]) -> Result<(), String> {
        if *self.fail_saves.borrow() {
            return Err("the state store refuses writes".to_string());
        }
        self.records
            .borrow_mut()
            .insert(key.to_string(), bytes.to_vec());
        *self.saves.borrow_mut() += 1;
        Ok(())
    }

    fn remove(&mut self, key: &str) -> Result<(), String> {
        self.records.borrow_mut().remove(key);
        Ok(())
    }

    fn keys(&mut self) -> Result<Vec<String>, String> {
        Ok(self.records.borrow().keys().cloned().collect())
    }
}
