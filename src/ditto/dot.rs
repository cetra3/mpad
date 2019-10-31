use std::cmp::max;
use std::collections::HashMap;

use serde_derive::{Deserialize, Serialize};

use super::error::Error;

pub type SiteId = u32;
pub type Counter = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Dot {
    pub site_id: SiteId,
    pub counter: Counter,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Summary(#[serde(with = "self")] HashMap<u32, u32>);

impl Dot {
    pub fn new(site_id: SiteId, counter: Counter) -> Self {
        Dot { site_id, counter }
    }
}

impl Summary {
    pub fn get(&self, site_id: SiteId) -> Counter {
        *self.0.get(&site_id).unwrap_or(&0)
    }

    pub fn get_dot(&mut self, site_id: SiteId) -> Dot {
        let counter = self.increment(site_id);
        Dot { site_id, counter }
    }

    pub fn increment(&mut self, site_id: SiteId) -> Counter {
        let entry = self.0.entry(site_id).or_insert(0);
        *entry += 1;
        *entry
    }

    pub fn contains(&self, dot: &Dot) -> bool {
        match self.0.get(&dot.site_id) {
            Some(counter) => *counter >= dot.counter,
            None => false,
        }
    }

    pub fn contains_pair(&self, site_id: u32, counter: u32) -> bool {
        match self.0.get(&site_id) {
            Some(site_counter) => *site_counter >= counter,
            None => false,
        }
    }

    pub fn insert(&mut self, dot: Dot) {
        let entry = self.0.entry(dot.site_id).or_insert(dot.counter);
        *entry = max(*entry, dot.counter);
    }

    pub fn insert_pair(&mut self, site_id: u32, counter: u32) {
        let entry = self.0.entry(site_id).or_insert(counter);
        *entry = max(*entry, counter);
    }

    pub fn merge(&mut self, other: &Summary) {
        for (site_id, counter) in &other.0 {
            let site_id = *site_id;
            let counter = *counter;
            let entry = self.0.entry(site_id).or_insert(counter);
            *entry = max(*entry, counter);
        }
    }

    pub fn add_site_id(&mut self, site_id: SiteId) {
        if let Some(counter) = self.0.remove(&0) {
            self.0.insert(site_id, counter);
        }
    }

    pub fn validate_no_unassigned_sites(&self) -> Result<(), Error> {
        if self.0.contains_key(&0) {
            Err(Error::InvalidSiteId)
        } else {
            Ok(())
        }
    }
}

use serde::de::{SeqAccess, Visitor};
use serde::ser::SerializeSeq;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use std::fmt;
use std::hash::Hash;
use std::marker::PhantomData;

pub fn serialize<K, V, S>(data: &HashMap<K, V>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
    K: Hash + Eq + Serialize,
    V: Serialize,
{
    let mut seq = serializer.serialize_seq(Some(data.len()))?;
    for kv_pair in data.iter() {
        seq.serialize_element(&kv_pair)?;
    }
    seq.end()
}

pub fn deserialize<'de, K, V, D>(deserializer: D) -> Result<HashMap<K, V>, D::Error>
where
    D: Deserializer<'de>,
    K: Hash + Eq + Deserialize<'de>,
    V: Deserialize<'de>,
{
    struct HashMapVisitor<K: Hash + Eq, V> {
        marker: PhantomData<HashMap<K, V>>,
    }

    impl<K: Hash + Eq, V> HashMapVisitor<K, V> {
        fn new() -> Self {
            HashMapVisitor {
                marker: PhantomData,
            }
        }
    }

    impl<'de, K, V> Visitor<'de> for HashMapVisitor<K, V>
    where
        K: Hash + Eq + Deserialize<'de>,
        V: Deserialize<'de>,
    {
        type Value = HashMap<K, V>;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("a list of (K, Vec<V>) tuples")
        }

        fn visit_seq<Vis>(self, mut visitor: Vis) -> Result<Self::Value, Vis::Error>
        where
            Vis: SeqAccess<'de>,
        {
            let mut hash_map = HashMap::with_capacity(visitor.size_hint().unwrap_or(0));
            while let Some((key, values)) = visitor.next_element()? {
                hash_map.insert(key, values);
            }
            Ok(hash_map)
        }
    }

    deserializer.deserialize_seq(HashMapVisitor::new())
}
