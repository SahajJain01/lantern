use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Claim {
    New(u32),
    Existing(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientHandle {
    pub win_id: u32,
}

#[derive(Default)]
pub struct Registry {
    next_id: u32,
    by_pkg_user: HashMap<String, u32>,
    windows: HashMap<u32, String>,
}

impl Registry {
    pub fn claim(&mut self, pkg_user: &str) -> Claim {
        if let Some(&id) = self.by_pkg_user.get(pkg_user) {
            return Claim::Existing(id);
        }
        self.next_id = self
            .next_id
            .checked_add(1)
            .expect("window id space exhausted");
        let id = self.next_id;
        self.by_pkg_user.insert(pkg_user.to_owned(), id);
        self.windows.insert(id, pkg_user.to_owned());
        Claim::New(id)
    }

    pub fn release(&mut self, win_id: u32) {
        if let Some(pkg_user) = self.windows.remove(&win_id) {
            self.by_pkg_user.remove(&pkg_user);
        }
    }

    pub fn route(&self, win_id: u32) -> Option<ClientHandle> {
        self.windows
            .contains_key(&win_id)
            .then_some(ClientHandle { win_id })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_second_claim_is_existing() {
        let mut registry = Registry::default();
        assert_eq!(registry.claim("com.example:0"), Claim::New(1));
        assert_eq!(registry.claim("com.example:0"), Claim::Existing(1));
        assert_eq!(registry.route(1), Some(ClientHandle { win_id: 1 }));
    }

    #[test]
    fn registry_ids_monotonic_after_release() {
        let mut registry = Registry::default();
        assert_eq!(registry.claim("com.one:0"), Claim::New(1));
        assert_eq!(registry.claim("com.two:0"), Claim::New(2));
        registry.release(1);
        assert_eq!(registry.route(1), None);
        assert_eq!(registry.claim("com.one:0"), Claim::New(3));
        assert_eq!(registry.route(1), None);
    }
}
