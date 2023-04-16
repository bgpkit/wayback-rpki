use crate::{RoaEntry, RoaHistoryEntry};
use chrono::{Duration, NaiveDate};
use ipnetwork::IpNetwork;
use std::collections::{Bound, HashMap};

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct RoasTable {
    roa_history_map: HashMap<(String, IpNetwork, i32, u32), Vec<NaiveDate>>,
}

impl RoasTable {
    pub fn new() -> RoasTable {
        RoasTable {
            roa_history_map: Default::default(),
        }
    }

    pub fn insert_entry(&mut self, roa_entry: &RoaEntry) {
        let id = (
            roa_entry.tal.to_owned(),
            roa_entry.prefix.to_owned(),
            roa_entry.max_len.to_owned(),
            roa_entry.asn,
        );
        let entry = self.roa_history_map.entry(id).or_insert(vec![]);
        entry.push(roa_entry.date);
    }

    pub fn merge_tables(tables: Vec<RoasTable>) -> RoasTable {
        let mut merged_map: HashMap<(String, IpNetwork, i32, u32), Vec<NaiveDate>> = HashMap::new();
        for table in tables {
            for (key, value) in table.roa_history_map {
                let vec = merged_map.entry(key).or_insert(vec![]);
                vec.extend(value);
            }
        }

        RoasTable {
            roa_history_map: merged_map,
        }
    }

    fn build_date_ranges(dates: &Vec<NaiveDate>) -> Vec<(Bound<NaiveDate>, Bound<NaiveDate>)> {
        if dates.is_empty() {
            return vec![];
        }

        if dates.len() == 1 {
            return vec![(Bound::Included(dates[0]), Bound::Included(dates[0]))];
        }

        let mut ranges = vec![];
        let mut cur = dates[0];
        let mut prev = dates[0];
        for i in 1..dates.len() {
            if dates[i] == prev + Duration::days(1) {
                // continue moving on
                prev = dates[i];
                // last one
                if i == dates.len() - 1 {
                    ranges.push((Bound::Included(cur), Bound::Included(prev)));
                }
            } else {
                // chain breaks
                ranges.push((Bound::Included(cur), Bound::Included(prev)));
                cur = dates[i];
                prev = dates[i];
                if i == dates.len() - 1 {
                    ranges.push((Bound::Included(cur), Bound::Included(prev)));
                }
            }
        }

        ranges
    }

    pub fn export_to_history(&self) -> Vec<RoaHistoryEntry> {
        let mut entries = vec![];
        for ((tal, prefix, max_len, asn), dates) in &self.roa_history_map {
            let mut new_dates = dates.clone();
            new_dates.sort();
            let date_ranges = Self::build_date_ranges(&new_dates);
            entries.push(RoaHistoryEntry {
                tal: tal.clone(),
                prefix: prefix.to_owned(),
                max_len: max_len.to_owned(),
                asn: *asn as i64,
                date_ranges,
            });
        }
        entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn test_insert() {
        let mut table = RoasTable::new();

        table.insert_entry(&RoaEntry {
            tal: "test_nic".to_string(),
            prefix: IpNetwork::from_str("0.0.1.0/24").unwrap(),
            max_len: 24,
            asn: 1234,
            date: NaiveDate::from_ymd_opt(2021, 1, 1).unwrap(),
        });

        table.insert_entry(&RoaEntry {
            tal: "test_nic".to_string(),
            prefix: IpNetwork::from_str("0.0.1.0/24").unwrap(),
            max_len: 24,
            asn: 1234,
            date: NaiveDate::from_ymd_opt(2022, 1, 2).unwrap(),
        });

        table.insert_entry(&RoaEntry {
            tal: "test_nic".to_string(),
            prefix: IpNetwork::from_str("0.0.1.0/24").unwrap(),
            max_len: 24,
            asn: 1234,
            date: NaiveDate::from_ymd_opt(2022, 1, 1).unwrap(),
        });

        table.insert_entry(&RoaEntry {
            tal: "test_nic".to_string(),
            prefix: IpNetwork::from_str("0.0.2.0/24").unwrap(),
            max_len: 24,
            asn: 1234,
            date: NaiveDate::from_ymd_opt(2022, 1, 1).unwrap(),
        });
    }

    #[test]
    fn test_merge_tables() {
        let mut table = RoasTable::new();
        table.insert_entry(&RoaEntry {
            tal: "test_nic".to_string(),
            prefix: IpNetwork::from_str("0.0.1.0/24").unwrap(),
            max_len: 24,
            asn: 1234,
            date: NaiveDate::from_ymd_opt(2022, 1, 1).unwrap(),
        });

        let mut table2 = RoasTable::new();
        table2.insert_entry(&RoaEntry {
            tal: "test_nic".to_string(),
            prefix: IpNetwork::from_str("0.0.2.0/24").unwrap(),
            max_len: 24,
            asn: 1234,
            date: NaiveDate::from_ymd_opt(2022, 1, 1).unwrap(),
        });
        table2.insert_entry(&RoaEntry {
            tal: "test_nic".to_string(),
            prefix: IpNetwork::from_str("0.0.1.0/24").unwrap(),
            max_len: 24,
            asn: 1234,
            date: NaiveDate::from_ymd_opt(2022, 1, 2).unwrap(),
        });

        let new_table = RoasTable::merge_tables(vec![table, table2]);
    }

    #[test]
    fn test_export() {
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .init();
        let mut table = RoasTable::new();
        table.insert_entry(&RoaEntry {
            tal: "test_nic".to_string(),
            prefix: IpNetwork::from_str("0.0.1.0/24").unwrap(),
            max_len: 24,
            asn: 1234,
            date: NaiveDate::from_ymd_opt(2021, 1, 1).unwrap(),
        });

        table.insert_entry(&RoaEntry {
            tal: "test_nic".to_string(),
            prefix: IpNetwork::from_str("0.0.1.0/24").unwrap(),
            max_len: 24,
            asn: 1234,
            date: NaiveDate::from_ymd_opt(2022, 1, 4).unwrap(),
        });

        table.insert_entry(&RoaEntry {
            tal: "test_nic".to_string(),
            prefix: IpNetwork::from_str("0.0.1.0/24").unwrap(),
            max_len: 24,
            asn: 1234,
            date: NaiveDate::from_ymd_opt(2022, 1, 2).unwrap(),
        });

        table.insert_entry(&RoaEntry {
            tal: "test_nic".to_string(),
            prefix: IpNetwork::from_str("0.0.1.0/24").unwrap(),
            max_len: 24,
            asn: 1234,
            date: NaiveDate::from_ymd_opt(2022, 1, 1).unwrap(),
        });

        let history = table.export_to_history();
    }
}
