use std::fmt;

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct GeneratedId(String);

impl GeneratedId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for GeneratedId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

pub trait IdProvider {
    fn next_id(&mut self, prefix: &str) -> GeneratedId;
}

#[derive(Debug, Default)]
pub struct SequentialIdProvider {
    next: u64,
}

impl SequentialIdProvider {
    pub fn new() -> Self {
        Self { next: 0 }
    }
}

impl IdProvider for SequentialIdProvider {
    fn next_id(&mut self, prefix: &str) -> GeneratedId {
        self.next += 1;
        GeneratedId::new(format!("{prefix}_{:04}", self.next))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequential_provider_is_deterministic() {
        let mut ids = SequentialIdProvider::new();
        assert_eq!(ids.next_id("op").as_str(), "op_0001");
        assert_eq!(ids.next_id("op").as_str(), "op_0002");
    }
}
