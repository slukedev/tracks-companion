//! The device registry: the app's own device ids and the devices each was restored from, so
//! a restored device inherits what its predecessors owned (sessions here; scrobbling, which
//! the server leaves to the clients).

use std::collections::{HashMap, HashSet};

use trackside_core::{DeviceId, Millis};

/// A device as registered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CloudDevice {
    pub device_id: String,
    /// The devices this one was restored from, most recent first as the client sent them.
    pub predecessors: Vec<String>,
    pub name: Option<String>,
    pub platform: Option<String>,
    /// The server device (signed-in install) that registered it, while it exists.
    pub server_device: Option<DeviceId>,
    pub registered_at: Millis,
    pub updated_at: Millis,
}

/// Who was restored from whom, for one user.
#[derive(Clone, Debug, Default)]
pub struct Registry {
    predecessors: HashMap<String, Vec<String>>,
}

impl Registry {
    pub fn new(devices: impl IntoIterator<Item = (String, Vec<String>)>) -> Self {
        Self {
            predecessors: devices.into_iter().collect(),
        }
    }

    /// Whether `device` is `owner`, or was restored (directly or through others) from it.
    pub fn succeeds(&self, device: &str, owner: &str) -> bool {
        if device == owner {
            return true;
        }
        let mut seen = HashSet::new();
        let mut queue = vec![device];
        while let Some(next) = queue.pop() {
            if !seen.insert(next) || seen.len() > 256 {
                continue;
            }
            for p in self.predecessors.get(next).into_iter().flatten() {
                if p == owner {
                    return true;
                }
                queue.push(p);
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restored_devices_inherit_through_chains_and_survive_cycles() {
        let registry = Registry::new([
            (String::from("C"), vec![String::from("B")]),
            (String::from("B"), vec![String::from("A")]),
            (String::from("X"), vec![String::from("Y")]),
            (String::from("Y"), vec![String::from("X")]),
        ]);
        assert!(registry.succeeds("A", "A"));
        assert!(registry.succeeds("C", "A"));
        assert!(
            !registry.succeeds("A", "C"),
            "ownership does not flow backwards"
        );
        assert!(!registry.succeeds("X", "A"));
        assert!(registry.succeeds("X", "Y"));
    }
}
